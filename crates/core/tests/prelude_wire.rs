//! End-to-end integration tests for Stage 2 session prelude:
//! - SI01: Three-protocol wire rendering and Anthropic `StablePrefix` `cache_control` anchor
//! - SI03: Dynamic section tail-append, reopen, read-only open, request_key idempotency
//! - SI04: End-to-end compaction folding and history expansion

#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/workspace_http.rs"]
mod http;

use causa::kernel::{BlockContent, ContentPart, ContextBlock};
use http::{Fixture, Reply};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tars_core::application::{PreparedCreate, SessionHost, text_input};
use tars_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};
use tars_core::permission::PermissionMode;
use tars_core::prelude::{
    BASE_HOST_CONTRACT, DynamicCustomSlot, DynamicSection, PRELUDE_BASE_SOURCE,
    PRELUDE_DYNAMIC_CUSTOM_SOURCE, PRELUDE_ENV_SOURCE, PRELUDE_SKILLS_SOURCE,
    PRELUDE_STATIC_CUSTOM_SOURCE, PRELUDE_USER_INSTRUCTIONS_SOURCE,
    PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE, PreludeDefinition, PreludeDiagnosticKind,
    StaticCustomSlot, has_prelude,
};
use tars_core::session::{SessionError, SubmitRequest, WorkState};

const PROTOCOLS: [(&str, Provider); 3] = [
    ("openai-chat", Provider::OpenAiChat),
    ("openai-responses", Provider::OpenAiResponses),
    ("anthropic", Provider::Anthropic),
];

fn write_skill(root: &Path, name: &str, desc: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\nBody for {name}.\n"),
    )
    .unwrap();
}

fn prelude_definition(
    provider: Provider,
    endpoint: &str,
    ws_root: &Path,
    cfg_dir: &Path,
    dyn_file: &Path,
    static_custom: Option<&str>,
) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "prelude-wire-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(20),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 3,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(ws_root.canonicalize().unwrap()),
        tools: vec![WorkspaceTool::ReadFile],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: PreludeDefinition {
            static_custom: static_custom.map(StaticCustomSlot::new),
            dynamic_custom: Some(DynamicCustomSlot::File(dyn_file.to_path_buf())),
            config_dir: Some(cfg_dir.to_path_buf()),
            skill_dirs: Vec::new(),
            include_workspace_skills: true,
            skills_enabled: true,
        },
    }
}

fn text_of(block: &ContextBlock) -> String {
    match block.content() {
        BlockContent::Message { parts, .. } => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.0.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_si01_three_protocol_wire_rendering_and_anthropic_cache_anchor() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    let cfg = temp.path().join("cfg");
    let dyn_file = temp.path().join("dyn.md");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("AGENTS.md"), "User rule: keep responses crisp.").unwrap();
    fs::write(ws.join("AGENTS.md"), "Workspace rule: verify every edit.").unwrap();
    fs::write(&dyn_file, "Sprint context: SI01 wire check.").unwrap();
    write_skill(&ws.join(".tars/skills"), "alpha-skill", "Alpha helper");

    let store = temp.path().join("store");
    let host = SessionHost::new(Some(store));

    for (name, provider) in PROTOCOLS {
        let fixture = Fixture::new(name, vec![Reply::Answer], None);
        let def = prelude_definition(
            provider,
            &fixture.endpoint,
            &ws,
            &cfg,
            &dyn_file,
            Some("Custom static system rule."),
        );
        let prepared = PreparedCreate::prepare_persistent(def.clone(), None).unwrap();
        let initial_blocks = prepared.context().blocks().to_vec();
        let initial_sources: Vec<&str> = initial_blocks
            .iter()
            .map(|b| b.meta().source.as_deref().unwrap())
            .collect();
        assert_eq!(
            initial_sources,
            vec![
                PRELUDE_BASE_SOURCE,
                PRELUDE_STATIC_CUSTOM_SOURCE,
                PRELUDE_USER_INSTRUCTIONS_SOURCE,
                PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
                PRELUDE_SKILLS_SOURCE,
                PRELUDE_ENV_SOURCE,
                PRELUDE_DYNAMIC_CUSTOM_SOURCE,
            ],
            "{name}"
        );

        // Verify prepare_persistent skips prepending if imported context already begins with prelude.
        let re_prepared =
            PreparedCreate::prepare_persistent(def.clone(), Some(prepared.context().clone()))
                .unwrap();
        assert_eq!(re_prepared.context().blocks(), prepared.context().blocks());

        // Verify prepare_transient emits identical prelude except capabilities omit `recall`.
        let transient_prepared = PreparedCreate::prepare_transient(def, None).unwrap();
        assert_eq!(transient_prepared.context().blocks().len(), 7);
        let env_transient = text_of(&transient_prepared.context().blocks()[5]);
        assert!(!env_transient.contains("recall"));

        let session = host.create(prepared).await.unwrap();
        let work = session
            .submit(SubmitRequest {
                request_key: format!("si01-{name}"),
                blocks: vec![text_input("Hello prelude wire")],
            })
            .unwrap()
            .work;
        let obs = session.wait(&work, Duration::from_secs(10)).await.unwrap();
        assert!(matches!(obs.state, WorkState::Finished(_)), "{name}");
        session.wait_saved().await.unwrap();

        let body: Value = serde_json::from_slice(&fixture.raw_requests.lock().unwrap()[0]).unwrap();
        match provider {
            Provider::Anthropic => {
                // Static Role::System blocks go to top-level `system` array in order,
                // and the 2nd cache_control anchor lands on `system[last]`, NOT on dynamic Role::User.
                let sys = body["system"].as_array().unwrap();
                assert_eq!(sys.len(), 2, "{name}: {body}");
                assert_eq!(sys[0]["text"], BASE_HOST_CONTRACT);
                assert!(sys[0].get("cache_control").is_none());
                assert_eq!(sys[1]["text"], "Custom static system rule.");
                assert_eq!(
                    sys[1]["cache_control"],
                    serde_json::json!({"type": "ephemeral"})
                );

                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "user");
                let content = messages[0]["content"].as_array().unwrap();
                let joined: String = content
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                let p_user = joined.find("<user_instructions>").unwrap();
                let p_ws = joined.find("<workspace_instructions>").unwrap();
                let p_skills = joined.find("<available_skills>").unwrap();
                let p_env = joined.find("<env>").unwrap();
                let p_dyn = joined.find("<dynamic_context>").unwrap();
                let p_turn = joined.find("Hello prelude wire").unwrap();
                assert!(
                    p_user < p_ws
                        && p_ws < p_skills
                        && p_skills < p_env
                        && p_env < p_dyn
                        && p_dyn < p_turn
                );
                // None of the 5 dynamic prelude parts may carry system cache_control;
                // (Anthropic StablePrefix places anchor #2 at system[last], not in dynamic Role::User,
                // and may place a conversation tail breakpoint only on the final message part.)
                assert_eq!(content.len(), 6);
                for part in &content[..5] {
                    assert!(
                        part.get("cache_control").is_none(),
                        "dynamic prelude part must not hold cache_control: {part}"
                    );
                }
            }
            Provider::OpenAiChat => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages[0]["content"], BASE_HOST_CONTRACT);
                assert_eq!(messages[1]["role"], "system");
                assert_eq!(messages[1]["content"], "Custom static system rule.");
                for idx in 2..=7 {
                    assert_eq!(messages[idx]["role"], "user", "idx {idx}: {messages:?}");
                }
                assert!(
                    messages[2]["content"]
                        .as_str()
                        .unwrap()
                        .contains("<user_instructions>")
                );
                assert!(
                    messages[3]["content"]
                        .as_str()
                        .unwrap()
                        .contains("<workspace_instructions>")
                );
                assert!(
                    messages[4]["content"]
                        .as_str()
                        .unwrap()
                        .contains("<available_skills>")
                );
                assert!(messages[5]["content"].as_str().unwrap().contains("<env>"));
                assert!(
                    messages[6]["content"]
                        .as_str()
                        .unwrap()
                        .contains("<dynamic_context>")
                );
                assert!(
                    messages[7]["content"]
                        .as_str()
                        .unwrap()
                        .contains("Hello prelude wire")
                );
            }
            Provider::OpenAiResponses => {
                let instructions = body["instructions"].as_str().unwrap();
                let base_at = instructions
                    .find("You are running inside the TARS agent host.")
                    .unwrap();
                let custom_at = instructions.find("Custom static system rule.").unwrap();
                assert!(base_at < custom_at);
                let input = body["input"].as_array().unwrap();
                assert_eq!(input.len(), 6, "{input:?}");
                assert!(input[0].to_string().contains("<user_instructions>"));
                assert!(input[1].to_string().contains("<workspace_instructions>"));
                assert!(input[2].to_string().contains("<available_skills>"));
                assert!(input[3].to_string().contains("<env>"));
                assert!(input[4].to_string().contains("<dynamic_context>"));
                assert!(input[5].to_string().contains("Hello prelude wire"));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_si03_dynamic_tail_append_reopen_readonly_open_and_request_key_idempotency() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    let cfg = temp.path().join("cfg");
    let dyn_file = temp.path().join("dyn.md");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&cfg).unwrap();
    fs::write(ws.join("AGENTS.md"), "WS rule v1").unwrap();
    fs::write(&dyn_file, "Dyn v1").unwrap();

    let replies = vec![
        Reply::Answer, // t1
        Reply::Answer, // t2 (unchanged)
        Reply::Answer, // t3 (ws + dyn modified)
        Reply::Answer, // t4 (ws + dyn deleted)
        Reply::Answer, // t5 (unchanged after delete)
        Reply::Answer, // t6 (skill added -> skills + env updated)
        Reply::Answer, // t7 (skill modified)
        Reply::Answer, // t8 (skill deleted -> skills removed + env updated)
        Reply::Answer, // t9 (permission mode switched)
        Reply::Answer, // t10 (after reopen + disk change)
        Reply::Answer, // t11 (tolerant invalid UTF-8)
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let store = temp.path().join("store");
    let host = SessionHost::new(Some(store));
    let def = prelude_definition(
        Provider::OpenAiChat,
        &fixture.endpoint,
        &ws,
        &cfg,
        &dyn_file,
        Some("Static custom frozen"),
    );

    let prepared = PreparedCreate::prepare_persistent(def, None).unwrap();
    let session = host.create(prepared).await.unwrap();
    let conv_id = session.target().conversation_id().clone();
    let initial_prefix = session.read_materials().unwrap().context.blocks()[..5].to_vec();

    async fn run_turn(session: &tars_core::application::ManagedSession, key: &str, msg: &str) {
        let work = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: vec![text_input(msg)],
            })
            .unwrap()
            .work;
        let obs = session.wait(&work, Duration::from_secs(10)).await.unwrap();
        assert!(matches!(obs.state, WorkState::Finished(_)));
        session.wait_saved().await.unwrap();
    }

    // Turn 1: baseline
    run_turn(&session, "t1", "turn 1").await;
    let blocks_t1 = session.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(&blocks_t1[..5], &initial_prefix[..]);
    assert_eq!(blocks_t1.len(), 8); // 5 prelude + user + tool_change + assistant

    // Turn 2: nothing changed -> 0 update blocks
    run_turn(&session, "t2", "turn 2").await;
    let blocks_t2 = session.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(blocks_t2.len(), blocks_t1.len() + 2);
    assert!(!blocks_t2[blocks_t1.len()..].iter().any(|b| {
        b.meta()
            .source
            .as_deref()
            .is_some_and(tars_core::prelude::is_prelude_update_source)
    }));

    // Turn 3: modify AGENTS.md & dynamic_context_file -> 2 update blocks right before user input
    fs::write(ws.join("AGENTS.md"), "WS rule v2").unwrap();
    fs::write(&dyn_file, "Dyn v2").unwrap();
    run_turn(&session, "t3", "turn 3").await;
    let blocks_t3 = session.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(
        &blocks_t3[..5],
        &initial_prefix[..],
        "initial prefix never modified"
    );
    let added_t3 = &blocks_t3[blocks_t2.len()..];
    assert_eq!(added_t3.len(), 4); // update:ws, update:dyn, user, assistant
    assert_eq!(
        added_t3[0].meta().source.as_deref(),
        Some(DynamicSection::WorkspaceInstructions.update_source())
    );
    assert!(text_of(&added_t3[0]).contains("<workspace_instructions updated=\"true\">"));
    assert_eq!(
        added_t3[1].meta().source.as_deref(),
        Some(DynamicSection::DynamicCustom.update_source())
    );
    assert!(text_of(&added_t3[1]).contains("<dynamic_context updated=\"true\">"));
    assert_eq!(text_of(&added_t3[2]), "turn 3");

    // Turn 4: delete AGENTS.md & dynamic_context_file -> tombstones
    fs::remove_file(ws.join("AGENTS.md")).unwrap();
    fs::remove_file(&dyn_file).unwrap();
    run_turn(&session, "t4", "turn 4").await;
    let blocks_t4 = session.read_materials().unwrap().context.blocks().to_vec();
    let added_t4 = &blocks_t4[blocks_t3.len()..];
    assert_eq!(added_t4.len(), 4);
    assert_eq!(
        text_of(&added_t4[0]),
        "<workspace_instructions removed=\"true\" />"
    );
    assert_eq!(
        text_of(&added_t4[1]),
        "<dynamic_context removed=\"true\" />"
    );

    // Turn 5: still deleted -> 0 update blocks
    run_turn(&session, "t5", "turn 5").await;
    let blocks_t5 = session.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(blocks_t5.len(), blocks_t4.len() + 2);

    // Turn 6: add SKILL.md -> skills updated AND env updated (capabilities gain `skills`)
    write_skill(&ws.join(".tars/skills"), "s1", "First skill");
    run_turn(&session, "t6", "turn 6").await;
    let blocks_t6 = session.read_materials().unwrap().context.blocks().to_vec();
    let added_t6 = &blocks_t6[blocks_t5.len()..];
    assert_eq!(added_t6.len(), 4);
    assert_eq!(
        added_t6[0].meta().source.as_deref(),
        Some(DynamicSection::Skills.update_source())
    );
    assert!(text_of(&added_t6[0]).contains("<available_skills updated=\"true\">"));
    assert_eq!(
        added_t6[1].meta().source.as_deref(),
        Some(DynamicSection::Env.update_source())
    );
    assert!(text_of(&added_t6[1]).contains("capabilities: workspace_read, recall, skills"));

    // Turn 7: modify SKILL.md -> only skills updated
    write_skill(&ws.join(".tars/skills"), "s1", "Updated skill desc");
    run_turn(&session, "t7", "turn 7").await;
    let blocks_t7 = session.read_materials().unwrap().context.blocks().to_vec();
    let added_t7 = &blocks_t7[blocks_t6.len()..];
    assert_eq!(added_t7.len(), 3);
    assert_eq!(
        added_t7[0].meta().source.as_deref(),
        Some(DynamicSection::Skills.update_source())
    );
    assert!(text_of(&added_t7[0]).contains("Updated skill desc"));

    // Turn 8: delete SKILL.md -> skills removed tombstone + env loses `skills`
    fs::remove_dir_all(ws.join(".tars/skills/s1")).unwrap();
    run_turn(&session, "t8", "turn 8").await;
    let blocks_t8 = session.read_materials().unwrap().context.blocks().to_vec();
    let added_t8 = &blocks_t8[blocks_t7.len()..];
    assert_eq!(added_t8.len(), 4);
    assert_eq!(
        text_of(&added_t8[0]),
        "<available_skills removed=\"true\" />"
    );
    assert_eq!(
        added_t8[1].meta().source.as_deref(),
        Some(DynamicSection::Env.update_source())
    );
    assert!(!text_of(&added_t8[1]).contains("skills"));

    // Turn 9: switch permission mode at runtime -> only update:env emitted
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    session.wait_saved().await.unwrap();
    run_turn(&session, "t9", "turn 9").await;
    let blocks_t9 = session.read_materials().unwrap().context.blocks().to_vec();
    let added_t9 = &blocks_t9[blocks_t8.len()..];
    assert_eq!(added_t9.len(), 3);
    assert_eq!(
        added_t9[0].meta().source.as_deref(),
        Some(DynamicSection::Env.update_source())
    );
    assert!(text_of(&added_t9[0]).contains("permission_mode: accept-edits"));

    // Close, modify AGENTS.md while closed, and reopen (read-only: zero writes / zero updates!).
    host.close(&session.target(), tars_core::application::ClosePolicy::Save)
        .await
        .unwrap();
    drop(session);

    fs::write(ws.join("AGENTS.md"), "WS rule after reopen").unwrap();
    let reopened = host.open(conv_id).await.unwrap();
    assert_eq!(
        reopened.read_materials().unwrap().context.blocks(),
        blocks_t9.as_slice(),
        "read-only open must not modify Context"
    );

    // First submit after reopen emits update for AGENTS.md change, and let's test request_key idempotency!
    let t10_req = SubmitRequest {
        request_key: "t10".into(),
        blocks: vec![text_input("turn 10")],
    };
    let work10 = reopened.submit(t10_req.clone()).unwrap().work;
    let obs10 = reopened
        .wait(&work10, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(obs10.state, WorkState::Finished(_)));
    reopened.wait_saved().await.unwrap();

    let blocks_t10 = reopened.read_materials().unwrap().context.blocks().to_vec();
    let added_t10 = &blocks_t10[blocks_t9.len()..];
    assert_eq!(added_t10.len(), 3);
    assert_eq!(
        added_t10[0].meta().source.as_deref(),
        Some(DynamicSection::WorkspaceInstructions.update_source())
    );
    assert!(text_of(&added_t10[0]).contains("WS rule after reopen"));

    // Modify AGENTS.md again, then replay exact t10_req -> must return SAME work_id and append 0 blocks!
    fs::write(ws.join("AGENTS.md"), "WS rule ignored by replay").unwrap();
    let replayed = reopened.submit(t10_req).unwrap();
    assert_eq!(replayed.work, work10);
    assert_eq!(
        reopened.read_materials().unwrap().context.blocks(),
        blocks_t10.as_slice()
    );

    // Same key with different blocks -> Conflict!
    let conflict = reopened
        .submit(SubmitRequest {
            request_key: "t10".into(),
            blocks: vec![text_input("different payload")],
        })
        .unwrap_err();
    assert!(matches!(
        conflict.reason,
        tars_core::application::ApplicationError::Session(SessionError::Conflict { .. })
    ));

    // Turn 11: invalid UTF-8 in AGENTS.md -> tolerant mode keeps previous snapshot, records diagnostic, succeeds!
    fs::write(ws.join("AGENTS.md"), [0xff, 0xfe, 0x00]).unwrap();
    run_turn(&reopened, "t11", "turn 11").await;
    let diags = reopened.prelude_diagnostics();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].kind, PreludeDiagnosticKind::InvalidUtf8);
    let blocks_t11 = reopened.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(blocks_t11.len(), blocks_t10.len() + 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_si04_e2e_compaction_folds_dynamic_updates_preserving_static_cache_and_history() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    let cfg = temp.path().join("cfg");
    let dyn_file = temp.path().join("dyn.md");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&cfg).unwrap();
    fs::write(ws.join("AGENTS.md"), "WS rule v1").unwrap();
    fs::write(&dyn_file, "Dyn v1").unwrap();

    let replies = vec![
        Reply::Answer,                            // turn 1
        Reply::Answer,                            // turn 2 (with WS v2 + permission mode)
        Reply::Answer, // turn 3 (stays in retained tail while turns 1..2 compact)
        Reply::Summary("Brief: folded updates."), // manual compact
        Reply::Answer, // turn 4 (clean folded prefix, 0 new updates)
    ];
    let fixture = Fixture::new("anthropic", replies, None);
    let store = temp.path().join("store");
    let host = SessionHost::new(Some(store));
    let mut def = prelude_definition(
        Provider::Anthropic,
        &fixture.endpoint,
        &ws,
        &cfg,
        &dyn_file,
        Some("Frozen static custom"),
    );
    def.context_window = Some(12_000);

    let prepared = PreparedCreate::prepare_persistent(def, None).unwrap();
    let session = host.create(prepared).await.unwrap();
    let static_ids: Vec<_> = session.read_materials().unwrap().context.blocks()[..2]
        .iter()
        .map(ContextBlock::id)
        .collect();

    // Turn 1
    let w1 = session
        .submit(SubmitRequest {
            request_key: "t1".into(),
            blocks: vec![text_input("first long turn ".repeat(40))],
        })
        .unwrap()
        .work;
    assert!(matches!(
        session
            .wait(&w1, Duration::from_secs(10))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    session.wait_saved().await.unwrap();

    // Modify WS AGENTS.md + switch permission mode before Turn 2
    fs::write(ws.join("AGENTS.md"), "WS rule v2 folded").unwrap();
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    session.wait_saved().await.unwrap();

    let w2 = session
        .submit(SubmitRequest {
            request_key: "t2".into(),
            blocks: vec![text_input("second long turn ".repeat(250))],
        })
        .unwrap()
        .work;
    assert!(matches!(
        session
            .wait(&w2, Duration::from_secs(10))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    session.wait_saved().await.unwrap();

    // Turn 3 stays in retained tail while Turn 2 + Turn 3 exceed retained_tail_budget,
    // ensuring turns 1..2 (containing the updates) fall inside head_end..k.
    let w3_pre = session
        .submit(SubmitRequest {
            request_key: "t3-pre".into(),
            blocks: vec![text_input("third long turn ".repeat(250))],
        })
        .unwrap()
        .work;
    assert!(matches!(
        session
            .wait(&w3_pre, Duration::from_secs(10))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    session.wait_saved().await.unwrap();

    // Manual compaction folds updates into dynamic prefix
    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();

    let after_compact = session.read_materials().unwrap().context;
    let blocks = after_compact.blocks();
    assert!(has_prelude(blocks));
    assert_eq!(blocks[0].id(), static_ids[0]);
    assert_eq!(blocks[1].id(), static_ids[1]);

    let ws_folded = blocks
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE))
        .unwrap();
    assert!(text_of(ws_folded).contains("<workspace_instructions>\nWS rule v2 folded"));
    assert!(!text_of(ws_folded).contains("updated=\"true\""));

    let env_folded = blocks
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(PRELUDE_ENV_SOURCE))
        .unwrap();
    assert!(text_of(env_folded).contains("permission_mode: accept-edits"));

    // No update blocks remain in the compacted Context
    assert!(!blocks.iter().any(|b| {
        b.meta()
            .source
            .as_deref()
            .is_some_and(tars_core::prelude::is_prelude_update_source)
    }));

    // Turn 3 with unchanged state must append 0 update blocks
    let before_t3_len = blocks.len();
    let w3 = session
        .submit(SubmitRequest {
            request_key: "t3".into(),
            blocks: vec![text_input("turn 3 after fold")],
        })
        .unwrap()
        .work;
    assert!(matches!(
        session
            .wait(&w3, Duration::from_secs(10))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    session.wait_saved().await.unwrap();
    let after_t3 = session.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(after_t3.len(), before_t3_len + 2);

    // Check Anthropic wire body of Turn 3: static system[last] cache_control intact, dynamic prefix has folded values
    let raw_bodies = fixture.raw_requests.lock().unwrap();
    let t3_body: Value = serde_json::from_slice(raw_bodies.last().unwrap()).unwrap();
    let sys = t3_body["system"].as_array().unwrap();
    assert_eq!(sys.len(), 2);
    assert_eq!(
        sys[1]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    let t3_msgs = serde_json::to_string(&t3_body["messages"]).unwrap();
    assert!(t3_msgs.contains("WS rule v2 folded"));
    assert!(t3_msgs.contains("permission_mode: accept-edits"));
    assert!(!t3_msgs.contains("WS rule v1"));

    // Expanded conversation preserves v1 dynamic prefix as Original + updates inside Summarized
    let expanded = session.expanded_conversation(None).unwrap();
    assert!(expanded.unreadable_records.is_empty());
    let ws_v1_pos = expanded
        .blocks
        .iter()
        .position(|b| text_of(&b.block).contains("WS rule v1"))
        .unwrap();
    assert_eq!(
        expanded.blocks[ws_v1_pos].status,
        tars_core::compact::BlockStatus::Original
    );
    let upd_pos = expanded
        .blocks
        .iter()
        .position(|b| text_of(&b.block).contains("<workspace_instructions updated=\"true\">"))
        .unwrap();
    assert_eq!(
        expanded.blocks[upd_pos].status,
        tars_core::compact::BlockStatus::Summarized(outcome.record.record_id)
    );
}
