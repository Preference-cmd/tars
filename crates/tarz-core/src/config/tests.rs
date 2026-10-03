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
