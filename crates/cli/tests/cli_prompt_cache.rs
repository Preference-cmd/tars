use serde_json::Value;
use std::fs;
use std::process::Command;

const KEY: &str = "fixture-secret";

fn report(output: &std::process::Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(stdout).unwrap()
}

#[test]
fn test_be20_cli_prompt_cache_flags_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // 1. Create with --prompt-cache off
    let out_off = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--store",
            store.to_str().unwrap(),
            "--json",
            "--provider",
            "openai-chat",
            "--model",
            "model-off",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--prompt-cache",
            "off",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(out_off.status.success(), "{out_off:?}");
    let id_off = report(&out_off)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let show_off = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "show",
            &id_off,
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(show_off.status.success(), "{show_off:?}");
    assert_eq!(report(&show_off)["prompt_cache"], false);

    // 2. Create with --prompt-cache on
    let out_on = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--store",
            store.to_str().unwrap(),
            "--json",
            "--provider",
            "openai-chat",
            "--model",
            "model-on",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--prompt-cache",
            "on",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(out_on.status.success(), "{out_on:?}");
    let id_on = report(&out_on)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let show_on = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "show",
            &id_on,
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(show_on.status.success(), "{show_on:?}");
    assert_eq!(report(&show_on)["prompt_cache"], true);

    // 3. TOML profile prompt_cache = false overridden by CLI --prompt-cache on
    let config_path = dir.path().join("config.toml");
    let toml = format!(
        r#"version = 1
store = '{}'
default_profile = 'local'
[profiles.local]
provider = 'openai-chat'
model = 'profile-model'
api_key_env = 'TARS_TEST_KEY'
prompt_cache = false
"#,
        store.display()
    );
    fs::write(&config_path, toml).unwrap();

    let out_override = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "--prompt-cache",
            "on",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(out_override.status.success(), "{out_override:?}");
    let id_override = report(&out_override)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let show_override = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "show",
            &id_override,
            "--config",
            config_path.to_str().unwrap(),
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(show_override.status.success(), "{show_override:?}");
    assert_eq!(report(&show_override)["prompt_cache"], true);
}
