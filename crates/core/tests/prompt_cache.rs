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
    let work = session
        .submit(SubmitRequest {
            request_key: "turn-1".into(),
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
    run_session_turn(&session, "Hello world prompt").await;
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
