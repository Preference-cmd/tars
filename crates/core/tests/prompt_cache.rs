#[path = "common/workspace_http.rs"]
mod workspace_http;

use causa::kernel::Context;
use tars_core::{
    application::{ManagedSession, PreparedCreate, SessionHost, text_input},
    config::{Config, ConfigSource, DefinitionOverrides},
    definition::{CredentialRef, Provider, SessionDefinition},
    session::*,
};
use workspace_http::{Fixture, Reply};

fn test_definition(
    provider: Provider,
    endpoint: Option<String>,
    prompt_cache: bool,
) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "fixture-model".into(),
        endpoint,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(120),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: if provider == Provider::Anthropic {
            Some(1024)
        } else {
            None
        },
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache,
        prelude: Default::default(),
    }
}

#[tokio::test]
async fn test_be20_prompt_cache_definition_default_and_overrides() {
    // 1. Serde default: missing prompt_cache defaults to true
    let mut value =
        serde_json::to_value(test_definition(Provider::OpenAiChat, None, true)).unwrap();
    value.as_object_mut().unwrap().remove("prompt_cache");
    let def: SessionDefinition = serde_json::from_value(value).unwrap();
    assert!(def.prompt_cache);

    // 2. Config layering: profile TOML and overrides
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let toml = r#"
version = 1
default_profile = "cached"

[profiles.cached]
provider = "openai-chat"
model = "cached-model"
api_key_env = "PATH"
prompt_cache = true

[profiles.uncached]
provider = "openai-chat"
model = "uncached-model"
api_key_env = "PATH"
prompt_cache = false
"#;
    std::fs::write(&toml_path, toml).unwrap();
    let config = Config::load(ConfigSource::File(toml_path)).unwrap();

    // Default profile has prompt_cache = true
    let def_cached = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert!(def_cached.prompt_cache);

    // Override default profile with prompt_cache = false
    let def_cached_off = config
        .definition(
            None,
            DefinitionOverrides {
                prompt_cache: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!def_cached_off.prompt_cache);

    // Uncached profile has prompt_cache = false
    let def_uncached = config
        .definition(Some("uncached"), DefinitionOverrides::default())
        .unwrap();
    assert!(!def_uncached.prompt_cache);

    // Override uncached profile with prompt_cache = true
    let def_uncached_on = config
        .definition(
            Some("uncached"),
            DefinitionOverrides {
                prompt_cache: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(def_uncached_on.prompt_cache);

    // 3. Persistence and reopen preserves prompt_cache setting
    let host = SessionHost::new(Some(dir.path().to_path_buf()));

    for expected in [false, true] {
        let def = test_definition(Provider::OpenAiChat, None, expected);
        let session = host
            .create(PreparedCreate::persistent(def, Context::new()))
            .await
            .unwrap();
        assert_eq!(session.definition().prompt_cache, expected);

        let id = session.target().conversation_id().clone();
        drop(session);

        let reopened = host.open(id).await.unwrap();
        assert_eq!(reopened.definition().prompt_cache, expected);
    }
}

async fn run_session_turn(session: &ManagedSession, text: &str) {
    run_session_turn_with_key(session, "turn-1", text).await;
}

async fn run_session_turn_with_key(session: &ManagedSession, key: &str, text: &str) {
    let work = session
        .submit(SubmitRequest {
            request_key: key.into(),
            blocks: vec![text_input(text)],
        })
        .unwrap()
        .work;
    let obs = session
        .wait(&work, std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(obs.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();
}

/// Raw bodies of one turn request and one manual compaction brief request.
async fn turn_and_brief_bodies(name: &str, provider: Provider, prompt_cache: bool) -> Vec<Vec<u8>> {
    let fixture = Fixture::new(
        name,
        vec![Reply::Answer, Reply::Summary("Goal: greet the user.")],
        None,
    );
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(dir.path().to_path_buf()));
    let definition = test_definition(provider, Some(fixture.endpoint.clone()), prompt_cache);
    let session = host
        .create(PreparedCreate::persistent(definition, Context::new()))
        .await
        .unwrap();
    run_session_turn(&session, &"Hello world prompt. ".repeat(30)).await;
    let outcome = session.compact().await;
    assert!(
        outcome.is_ok(),
        "{name}: {outcome:?} after {:?}",
        fixture.requests.lock().unwrap()
    );
    let bodies = fixture.raw_requests.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "{name}: one turn and one brief request");
    bodies
}

/// The body with every `cache_control` member removed.
fn without_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            map.values_mut().for_each(without_cache_control);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(without_cache_control),
        _ => {}
    }
}

#[tokio::test]
async fn test_be20_wire_request_bodies_across_all_three_protocols() {
    for (name, provider) in [
        ("openai-chat", Provider::OpenAiChat),
        ("openai-responses", Provider::OpenAiResponses),
        ("anthropic", Provider::Anthropic),
    ] {
        let off = turn_and_brief_bodies(name, provider, false).await;
        let on = turn_and_brief_bodies(name, provider, true).await;
        for (index, (off, on)) in off.iter().zip(&on).enumerate() {
            let off_text = String::from_utf8_lossy(off);
            let on_text = String::from_utf8_lossy(on);
            // Off never asks for cache breakpoints, in turns or briefs.
            assert!(
                !off_text.contains("cache_control"),
                "{name} #{index}: {off_text}"
            );
            match provider {
                // OpenAI servers cache on their own: the switch changes no byte.
                Provider::OpenAiChat | Provider::OpenAiResponses => {
                    assert_eq!(off_text, on_text, "{name} #{index}");
                }
                // Anthropic gains breakpoints and nothing else.
                Provider::Anthropic => {
                    assert!(
                        on_text.contains("cache_control"),
                        "{name} #{index}: {on_text}"
                    );
                    let mut on: serde_json::Value = serde_json::from_slice(on).unwrap();
                    without_cache_control(&mut on);
                    let off: serde_json::Value = serde_json::from_slice(off).unwrap();
                    assert_eq!(on, off, "{name} #{index}");
                }
            }
        }
    }
}

/// Extracts the rendered system prefix, tools declaration, and conversation items
/// (with Anthropic `cache_control` markers stripped) from a raw wire body.
fn extract_wire_head(
    provider: Provider,
    raw: &[u8],
) -> (serde_json::Value, serde_json::Value, Vec<serde_json::Value>) {
    let mut body: serde_json::Value = serde_json::from_slice(raw).unwrap();
    if provider == Provider::Anthropic {
        without_cache_control(&mut body);
    }
    let tools = body
        .get("tools")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match provider {
        Provider::OpenAiChat => {
            let messages = body["messages"].as_array().unwrap().clone();
            let leading_sys_len = messages
                .iter()
                .take_while(|msg| msg["role"] == "system")
                .count();
            let system = serde_json::Value::Array(messages[..leading_sys_len].to_vec());
            let items = messages[leading_sys_len..].to_vec();
            (system, tools, items)
        }
        Provider::OpenAiResponses => {
            let system = body
                .get("instructions")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let items = body["input"].as_array().unwrap().clone();
            (system, tools, items)
        }
        Provider::Anthropic => {
            let system = body
                .get("system")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let items = body["messages"].as_array().unwrap().clone();
            (system, tools, items)
        }
    }
}

#[tokio::test]
async fn test_be20_frame_head_is_byte_identical_between_merges() {
    for (name, provider) in [
        ("openai-chat", Provider::OpenAiChat),
        ("openai-responses", Provider::OpenAiResponses),
        ("anthropic", Provider::Anthropic),
    ] {
        let fixture = Fixture::new(
            name,
            vec![
                Reply::Answer,                             // turn 1
                Reply::Summary("Brief S1: first phase."),  // compact 1 -> S1
                Reply::Answer,                             // turn 2 (head = prefix + S1)
                Reply::Answer,                             // turn 3 (head = prefix + S1)
                Reply::Summary("Brief S2: second phase."), // compact 2 -> S1, S2 (no merge)
                Reply::Answer,                             // turn 4 (head = prefix + S1 + S2)
            ],
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(dir.path().to_path_buf()));
        let mut definition = test_definition(provider, Some(fixture.endpoint.clone()), true);
        // Large window so two small briefs stay well under the chain budget and never merge.
        definition.context_window = Some(100_000);
        let initial = Context::from_blocks(vec![
            causa::kernel::ContextBlock::new(
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                causa::kernel::BlockContent::text(causa::kernel::Role::System, "System rule one."),
                causa::kernel::BlockMeta::default(),
            ),
            causa::kernel::ContextBlock::new(
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                causa::kernel::BlockContent::text(causa::kernel::Role::System, "System rule two."),
                causa::kernel::BlockMeta::default(),
            ),
        ])
        .unwrap();
        let session = host
            .create(PreparedCreate::persistent(definition, initial))
            .await
            .unwrap();

        run_session_turn_with_key(&session, "t1", &"First turn payload. ".repeat(40)).await;
        let first = session.compact().await.expect("first manual compact");
        assert!(
            first.merged.is_none(),
            "{name}: first compact must not merge"
        );
        session.wait_saved().await.unwrap();

        run_session_turn_with_key(&session, "t2", &"Second turn payload. ".repeat(40)).await;
        run_session_turn_with_key(&session, "t3", &"Third turn payload. ".repeat(40)).await;

        let second = session.compact().await.expect("second manual compact");
        assert!(
            second.merged.is_none(),
            "{name}: second compact must not merge"
        );
        session.wait_saved().await.unwrap();

        run_session_turn_with_key(&session, "t4", "Fourth turn short question.").await;

        let history = session.history();
        assert_eq!(history.len(), 2, "{name}: two summarize records, no merge");
        assert!(
            history
                .iter()
                .all(|entry| entry.kind == tars_core::compact::RewriteKind::Summarize),
            "{name}: expected only summarize records between merges"
        );

        let bodies = fixture.raw_requests.lock().unwrap().clone();
        assert_eq!(bodies.len(), 6, "{name}: 4 turns + 2 briefs");

        // Compare turn 2 (#2), turn 3 (#3), and turn 4 (#5) between merges.
        let (sys2, tools2, items2) = extract_wire_head(provider, &bodies[2]);
        let (sys3, tools3, items3) = extract_wire_head(provider, &bodies[3]);
        let (sys4, tools4, items4) = extract_wire_head(provider, &bodies[5]);

        assert_eq!(
            serde_json::to_vec(&sys2).unwrap(),
            serde_json::to_vec(&sys3).unwrap(),
            "{name}: system prefix must be byte-identical between turn 2 and turn 3"
        );
        assert_eq!(
            serde_json::to_vec(&sys2).unwrap(),
            serde_json::to_vec(&sys4).unwrap(),
            "{name}: system prefix must be byte-identical after second summary"
        );
        assert_eq!(
            serde_json::to_vec(&tools2).unwrap(),
            serde_json::to_vec(&tools3).unwrap(),
            "{name}: tools must be byte-identical between turn 2 and turn 3"
        );
        assert_eq!(
            serde_json::to_vec(&tools2).unwrap(),
            serde_json::to_vec(&tools4).unwrap(),
            "{name}: tools must be byte-identical after second summary"
        );

        // S1 is the first conversation block after the fixed prefix and must stay byte-identical
        // across turn 2, turn 3, and turn 4 (no merge happened). Anthropic coalesces adjacent user
        // blocks into one user message's `content` array, so S1 is `items[0]["content"][0]` there.
        let s1_slice = |items: &[serde_json::Value]| -> Vec<u8> {
            let value = match provider {
                Provider::Anthropic => &items[0]["content"][0],
                Provider::OpenAiChat | Provider::OpenAiResponses => &items[0],
            };
            serde_json::to_vec(value).unwrap()
        };
        let s1_t2 = s1_slice(&items2);
        let s1_t3 = s1_slice(&items3);
        let s1_t4 = s1_slice(&items4);
        assert!(
            String::from_utf8_lossy(&s1_t2).contains("Brief S1: first phase."),
            "{name}: first item after prefix should be S1"
        );
        assert_eq!(
            s1_t2, s1_t3,
            "{name}: S1 in frame head must be byte-identical between turn 2 and turn 3"
        );
        assert_eq!(
            s1_t2, s1_t4,
            "{name}: S1 in frame head must remain byte-identical after appending S2"
        );

        // Turn 4 must also contain S2 somewhere after S1 (Anthropic may merge adjacent user turns
        // into one message with multiple content blocks, so check the serialized items4).
        let items4_text = serde_json::to_string(&items4).unwrap();
        assert!(
            items4_text.contains("Brief S2: second phase."),
            "{name}: turn 4 should include S2 after S1"
        );
    }
}
