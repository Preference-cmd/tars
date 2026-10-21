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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn workspace_selection_is_explicit_resolved_and_frozen_with_empty_override() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("tools.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}\nworkspace_root = 'workspace'\ntools = ['read_file', 'search_text']\n",
            config().split("[profiles.second]").next().unwrap()
        ),
    )
    .unwrap();
    let created = json(
        cli(dir.path())
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .arg("--json")
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
    let first = &listing["sessions"][0]["definition"];
    assert_eq!(
        first["workspace_root"],
        workspace.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        first["tools"],
        serde_json::json!(["read_file", "search_text"])
    );
    let record = dir.path().join("configured-data/sessions").join(format!(
        "{}.json",
        created["conversation_id"].as_str().unwrap()
    ));
    let frozen = std::fs::read(&record).unwrap();
    let disabled = json(
        cli(dir.path())
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .args(["--tools", "", "--json"])
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
    let second = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["conversation_id"] == disabled["conversation_id"])
        .unwrap();
    assert_eq!(second["definition"]["tools"], serde_json::json!([]));
    assert_eq!(std::fs::read(record).unwrap(), frozen);
    // A workspace on its own never grants a tool capability.
    let store = dir.path().join("explicit");
    json(
        cli(dir.path())
            .args([
                "session",
                "create",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "fixture",
                "--api-key-env",
                "TARZ_CONFIG_TEST_KEY",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--store")
            .arg(&store)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let listing = json(
        cli(dir.path())
            .args(["session", "list", "--no-config", "--store"])
            .arg(store)
            .arg("--json")
            .output()
            .unwrap(),
    );
    assert_eq!(
        listing["sessions"][0]["definition"]["tools"],
        serde_json::json!([])
    );
}

#[test]
fn legacy_record_has_an_offline_non_overwriting_material_export_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let material = dir.path().join("initial.json");
    let context = causa::kernel::Context::from_blocks(vec![tarz_core::application::text_input(
        "legacy material retained",
    )])
    .unwrap();
    tarz_core::material::save_new(&material, &context).unwrap();
    let created = json(
        cli(dir.path())
            .args(["session", "create", "--no-config", "--store"])
            .arg(&store)
            .args([
                "--provider",
                "openai-chat",
                "--model",
                "fixture",
                "--api-key-env",
                "TARZ_CONFIG_TEST_KEY",
                "--load",
            ])
            .arg(&material)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();
    let record = store.join("sessions").join(format!("{id}.json"));
    let mut value: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    value["format"] = serde_json::json!("tarz-session-v1");
    for key in ["workspace_root", "tools", "tool_limits"] {
        value["definition"].as_object_mut().unwrap().remove(key);
    }
    std::fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    let original = std::fs::read(&record).unwrap();
    let reopened = cli(dir.path())
        .args(["session", "run", id, "--no-config", "--store"])
        .arg(&store)
        .args(["--input", "continue", "--json"])
        .output()
        .unwrap();
    assert_eq!(reopened.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("unsupported session format"));
    let exported = dir.path().join("export.json");
    let result = cli(dir.path())
        .env("TARZ_CONFIG", dir.path().join("missing.toml"))
        .env_remove("TARZ_CONFIG_TEST_KEY")
        .args(["session", "export-materials", "--record"])
        .arg(&record)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        tarz_core::material::load(&exported).unwrap().blocks(),
        context.blocks()
    );
    assert_eq!(std::fs::read(&record).unwrap(), original);
    let previous = std::fs::read(&exported).unwrap();
    let overwrite = cli(dir.path())
        .args(["session", "export-materials", "--record"])
        .arg(&record)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert_eq!(overwrite.status.code(), Some(1));
    assert_eq!(std::fs::read(exported).unwrap(), previous);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn wm10_definition_save_and_reopen_and_export_omits_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let ws_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let secret_env = "TARZ_WM10_SECRET";
    let secret_val = "wm10-secret-6f1c";

    let config_path = dir.path().join("config.toml");
    let config_toml = format!(
        r#"version = 1
store = "store"
recovery_dir = "recoveries"
default_profile = "wm10"

[profiles.wm10]
provider = "openai-chat"
model = "wm10-model"
api_key_env = "{secret_env}"
workspace_root = "{}"
tools = ["read_file", "write_file", "edit_file"]
permission_mode = "accept-edits"

[profiles.wm10.write_limits]
max_write_bytes = 524288
max_edit_file_bytes = 1048576
max_edit_segments = 16
"#,
        ws_dir.display()
    );
    std::fs::write(&config_path, config_toml).unwrap();

    let create_output = cli(dir.path())
        .env(secret_env, secret_val)
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();
    let created = json(create_output);
    let id = created["conversation_id"].as_str().unwrap();

    let record_path = dir.path().join("store/sessions").join(format!("{id}.json"));
    let record_bytes = std::fs::read(&record_path).unwrap();
    let record_str = String::from_utf8_lossy(&record_bytes);

    assert!(
        !record_str.contains(secret_val),
        "record bytes must not contain credential value"
    );
    assert!(
        record_str.contains(secret_env),
        "record must contain credential variable name reference"
    );

    let exported = dir.path().join("exported.json");
    let export_res = cli(dir.path())
        .env(secret_env, secret_val)
        .args(["session", "export-materials", "--record"])
        .arg(&record_path)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export_res.status.success(), "{export_res:?}");

    let export_stdout = String::from_utf8_lossy(&export_res.stdout);
    let export_stderr = String::from_utf8_lossy(&export_res.stderr);
    assert!(
        !export_stdout.contains(secret_val),
        "export-materials stdout must not contain credential value"
    );
    assert!(
        !export_stderr.contains(secret_val),
        "export-materials stderr must not contain credential value"
    );

    let exported_bytes = std::fs::read(&exported).unwrap();
    let exported_str = String::from_utf8_lossy(&exported_bytes);
    assert!(
        !exported_str.contains(secret_val),
        "exported materials must not contain credential value"
    );

    let list_output = cli(dir.path())
        .env(secret_env, secret_val)
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();
    let listing = json(list_output);
    let sessions = listing["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let def = &sessions[0]["definition"];
    assert_eq!(
        def["tools"],
        serde_json::json!(["read_file", "write_file", "edit_file"])
    );
    assert_eq!(def["permission_mode"], "accept-edits");
    assert_eq!(def["write_limits"]["max_write_bytes"], 524288);
    assert_eq!(def["write_limits"]["max_edit_file_bytes"], 1048576);
    assert_eq!(def["write_limits"]["max_edit_segments"], 16);

    let repair_res = cli(dir.path())
        .env(secret_env, secret_val)
        .args(["session", "repair", id, "--config"])
        .arg(&config_path)
        .args(["--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(repair_res.status.success(), "{repair_res:?}");
    let repair_val = json(repair_res);
    assert_eq!(repair_val["conversation_id"], id);
    assert_eq!(repair_val["status"], "not_needed");

    let host = tarz_core::application::SessionHost::new(Some(dir.path().join("store")));
    let host_list = host.list().await.unwrap();
    assert_eq!(host_list.len(), 1);
    let record_summary = host_list[0].record.as_ref().unwrap();
    assert_eq!(
        record_summary.definition.tools,
        vec![
            tarz_core::definition::WorkspaceTool::ReadFile,
            tarz_core::definition::WorkspaceTool::WriteFile,
            tarz_core::definition::WorkspaceTool::EditFile,
        ]
    );
    assert_eq!(
        record_summary.definition.permission_mode,
        Some(tarz_core::permission::PermissionMode::AcceptEdits)
    );
    assert_eq!(
        record_summary.definition.write_limits,
        Some(tarz_core::definition::WriteToolLimits {
            max_write_bytes: 524288,
            max_edit_file_bytes: 1048576,
            max_edit_segments: 16,
        })
    );
}
