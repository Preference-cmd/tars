use super::{Config, ConfigSource, DefinitionOverrides};
use crate::definition::{CredentialRef, Provider};

const CONFIG: &str = r#"
version = 1
store = "data"
recovery_dir = "recoveries"
default_profile = "local"
[profiles.local]
provider = "openai-chat"
model = "local-model"
api_key_env = "FIXTURE_TOKEN"
timeout_secs = 30
max_model_rounds = 3
"#;

#[test]
fn file_relative_paths_preset_and_explicit_fields_resolve_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(config.source(), Some(path.as_path()));
    assert_eq!(config.store_dir(), Some(dir.path().join("data").as_path()));
    assert_eq!(
        config.recovery_dir(),
        Some(dir.path().join("recoveries").as_path())
    );
    let definition = config
        .definition(
            None,
            DefinitionOverrides {
                model: Some("explicit".into()),
                timeout_secs: Some(17),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(definition.model, "explicit");
    assert_eq!(definition.provider, Provider::OpenAiChat);
    assert_eq!(definition.work_timeout_secs, Some(17));
    assert_eq!(definition.max_model_rounds, 3);
    assert_eq!(
        definition.credential,
        CredentialRef::Environment("FIXTURE_TOKEN".into())
    );
    std::fs::write(path, CONFIG.replace("local-model", "changed")).unwrap();
    assert_eq!(config.preset(None).unwrap().unwrap().model, "local-model");
    assert!(config.preset(Some("absent")).is_err());
}

#[test]
fn unknown_plaintext_key_invalid_values_and_missing_files_are_rejected_without_source_echo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for text in [
        CONFIG.replace(
            "api_key_env = \"FIXTURE_TOKEN\"",
            "api_key = \"secret-do-not-print\"",
        ),
        CONFIG.replace("version = 1", "version = 2"),
        CONFIG.replace("timeout_secs = 30", "timeout_secs = 0"),
        CONFIG.replace(
            "default_profile = \"local\"",
            "default_profile = \"missing\"",
        ),
        CONFIG.replace("store = \"data\"", "store = \"\""),
        "version = 1\ninvalid = \"secret-do-not-print\"".into(),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = Config::load(ConfigSource::File(path.clone()))
            .err()
            .expect("must reject")
            .to_string();
        assert!(!error.contains("secret-do-not-print"));
    }
    assert!(Config::load(ConfigSource::File(dir.path().join("missing.toml"))).is_err());
    assert!(
        Config::load(ConfigSource::Disabled)
            .unwrap()
            .preset(None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn workspace_profile_freezes_physical_root_and_explicit_empty_tools_disable_it() {
    use crate::definition::WorkspaceTool;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let file = directory.path().join("config.toml");
    let text = format!(
        "{CONFIG}\nworkspace_root = \"workspace\"\ntools = [\"read_file\", \"search_text\"]\n"
    );
    std::fs::write(&file, text).unwrap();
    let config = Config::load(ConfigSource::File(file)).unwrap();
    let frozen = config.definition(None, Default::default()).unwrap();
    assert_eq!(frozen.workspace_root, Some(root.canonicalize().unwrap()));
    assert_eq!(
        frozen.tools,
        vec![WorkspaceTool::ReadFile, WorkspaceTool::SearchText]
    );
    let disabled = config
        .definition(
            None,
            DefinitionOverrides {
                tools: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(disabled.tools.is_empty());
    assert_eq!(disabled.workspace_root, frozen.workspace_root);
    assert_eq!(disabled.tool_limits, frozen.tool_limits);
}

#[test]
fn workspace_selection_never_implicitly_enables_tools_and_rejects_missing_roots() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config::load(ConfigSource::Disabled).unwrap();
    let overrides = || DefinitionOverrides {
        provider: Some(Provider::OpenAiChat),
        model: Some("fixture".into()),
        ..Default::default()
    };
    let mut chosen = overrides();
    chosen.workspace_root = Some(directory.path().to_owned());
    let definition = config.definition(None, chosen).unwrap();
    assert!(definition.tools.is_empty());
    let mut missing = overrides();
    missing.workspace_root = Some(directory.path().join("absent"));
    assert!(matches!(
        config.definition(None, missing),
        Err(super::ConfigError::Workspace { .. })
    ));
    let mut forbidden = overrides();
    forbidden.tools = Some(vec![crate::definition::WorkspaceTool::ReadFile]);
    assert!(matches!(
        config.definition(None, forbidden),
        Err(super::ConfigError::Definition(
            crate::definition::DefinitionError::WorkspaceRequired
        ))
    ));
    assert!(super::parse_tools("read_file,read_file").is_err());
    assert!(super::parse_tools("execute_command").is_err());
    assert!(super::parse_tools("").unwrap().is_empty());
}

#[test]
fn explicit_workspace_resolution_freezes_current_directory() {
    const MARKER: &str = "TARZ_CONFIG_CWD_TEST_DESTINATION";
    if let Some(destination) = std::env::var_os(MARKER) {
        let initial = std::env::current_dir().unwrap().canonicalize().unwrap();
        let config = Config::load(ConfigSource::Disabled).unwrap();
        let definition = config
            .definition(
                None,
                DefinitionOverrides {
                    provider: Some(Provider::OpenAiChat),
                    model: Some("fixture".into()),
                    workspace_root: Some(".".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        std::env::set_current_dir(destination).unwrap();
        assert_eq!(definition.workspace_root, Some(initial));
        return;
    }
    let initial = tempfile::tempdir().unwrap();
    let later = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("config::tests::explicit_workspace_resolution_freezes_current_directory")
        .current_dir(initial.path())
        .env(MARKER, later.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn generation_settings_come_from_the_preset_unless_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = format!("{CONFIG}max_tokens = 4000\nreasoning_effort = \"low\"\n");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path)).unwrap();
    let preset = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(preset.max_tokens, Some(4000));
    assert_eq!(preset.reasoning_effort.as_deref(), Some("low"));
    assert!(preset.extra_body.is_empty());
    let overridden = config
        .definition(
            None,
            DefinitionOverrides {
                max_tokens: Some(9000),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(overridden.max_tokens, Some(9000));

    let unset = Config::load(ConfigSource::Disabled)
        .unwrap()
        .definition(
            None,
            DefinitionOverrides {
                provider: Some(Provider::Anthropic),
                model: Some("m".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(unset.max_tokens, None);
}

#[test]
fn effort_text_and_extra_request_fields_are_kept_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // A value no fixed list would contain, and a nested table.
    let text = format!(
        "{CONFIG}reasoning_effort = \"off\"\n[profiles.local.extra_body]\ntop_k = 40\n[profiles.local.extra_body.thinking]\ntype = \"adaptive\"\ndisplay = \"summarized\"\n"
    );
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path)).unwrap();
    let preset = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(preset.reasoning_effort.as_deref(), Some("off"));
    assert_eq!(
        serde_json::Value::Object(preset.extra_body.clone()),
        serde_json::json!({"top_k": 40, "thinking": {"type": "adaptive", "display": "summarized"}})
    );
    let generation = preset.run_options().generation;
    assert_eq!(generation.reasoning_effort.as_deref(), Some("off"));
    assert_eq!(generation.extra_body, preset.extra_body);

    // An explicit value replaces the preset's object as a whole.
    let mut replacement = serde_json::Map::new();
    replacement.insert("enable_thinking".into(), serde_json::json!(false));
    let overridden = config
        .definition(
            None,
            DefinitionOverrides {
                extra_body: Some(replacement.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(overridden.extra_body, replacement);

    // A definition saved before the field existed reads as empty.
    let mut saved = serde_json::to_value(&preset).unwrap();
    saved.as_object_mut().unwrap().remove("extra_body");
    let reread: crate::definition::SessionDefinition = serde_json::from_value(saved).unwrap();
    assert!(reread.extra_body.is_empty());
}
