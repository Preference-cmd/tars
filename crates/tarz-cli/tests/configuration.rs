//! Shared TOML is exercised through actual CLI processes, without live models.
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn config() -> &'static str {
    "version = 1\nstore = 'configured-data'\nrecovery_dir = '.'\ndefault_profile = 'first'\n[profiles.first]\nprovider = 'openai-chat'\nmodel = 'saved-model'\napi_key_env = 'TARZ_CONFIG_TEST_KEY'\ntimeout_secs = 17\n[profiles.second]\nprovider = 'anthropic'\nmodel = 'second-model'\napi_key_env = 'TARZ_CONFIG_TEST_KEY'\n"
}
fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    command
        .env_remove("TARZ_CONFIG")
        .env("XDG_CONFIG_HOME", home)
        .env("APPDATA", home)
        .env("TARZ_CONFIG_TEST_KEY", "fixture-only");
    command
}
fn json(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn explicit_config_and_overrides_share_paths_and_do_not_mutate_saved_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config()).unwrap();
    let created = json(
        cli(dir.path())
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();
    let record_path = dir
        .path()
        .join("configured-data/sessions")
        .join(format!("{id}.json"));
    let before = std::fs::read_to_string(&record_path).unwrap();
    assert!(before.contains("saved-model"));
    assert!(!before.contains("fixture-only"));
    let other = json(
        cli(dir.path())
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .args(["--profile", "second", "--model", "explicit-model", "--json"])
            .output()
            .unwrap(),
    );
    let listing = json(
        cli(dir.path())
            .args(["session", "list", "--config"])
            .arg(&config_path)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let record = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["conversation_id"] == other["conversation_id"])
        .unwrap();
    assert_eq!(record["definition"]["provider"], "anthropic");
    assert_eq!(record["definition"]["model"], "explicit-model");
    std::fs::write(
        &config_path,
        config().replace("saved-model", "changed-global-model"),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(record_path).unwrap(), before);
    let explicit_store = dir.path().join("explicit-data");
    let listed = json(
        cli(dir.path())
            .args(["session", "list", "--config"])
            .arg(&config_path)
            .arg("--store")
            .arg(explicit_store)
            .arg("--json")
            .output()
            .unwrap(),
    );
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);
}
#[test]
fn discovery_precedence_disabled_and_invalid_config_are_observable() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join(if cfg!(windows) { "Tarz" } else { "tarz" });
    std::fs::create_dir(&global).unwrap();
    std::fs::write(global.join("config.toml"), config()).unwrap();
    let default = json(
        cli(dir.path())
            .args(["session", "create", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        global
            .join("configured-data/sessions")
            .join(format!(
                "{}.json",
                default["conversation_id"].as_str().unwrap()
            ))
            .exists()
    );
    let env_path = dir.path().join("environment.toml");
    std::fs::write(&env_path, config().replace("saved-model", "env-model")).unwrap();
    let environment = json(
        cli(dir.path())
            .env("TARZ_CONFIG", &env_path)
            .args(["session", "create", "--json"])
            .output()
            .unwrap(),
    );
    let record = std::fs::read_to_string(dir.path().join("configured-data/sessions").join(
        format!("{}.json", environment["conversation_id"].as_str().unwrap()),
    ))
    .unwrap();
    assert!(record.contains("env-model"));
    let missing = dir.path().join("missing.toml");
    let output = cli(dir.path())
        .env("TARZ_CONFIG", missing)
        .args(["session", "list", "--no-config", "--store"])
        .arg(dir.path().join("empty"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success());
    let invalid = dir.path().join("invalid.toml");
    std::fs::write(&invalid, "version = 1\napi_key = 'secret-must-not-appear'").unwrap();
    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(invalid)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid TOML"));
    assert!(!error.contains("secret-must-not-appear"));
}
