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

fn mcp_config(workspace: &Path) -> String {
    format!(
        r#"version = 1
store = "store"
recovery_dir = "."
default_profile = "local"

[mcp_servers.git]
command = "uvx"
args = ["mcp-server-git", "--repository", "."]
env = {{ LOG_LEVEL = "warn" }}
env_vars = ["GIT_AUTHOR_NAME"]
startup_timeout_sec = 12
tool_timeout_sec = 120
enabled_tools = ["git_status", "git_diff"]
approval = "writes"
required = true

[mcp_servers.git.tools.git_status]
approval = "auto"

[mcp_servers.docs]
url = "https://example.invalid/mcp"
bearer_token_env_var = "DOCS_MCP_TOKEN"
disabled_tools = ["danger"]

[mcp_servers.docs.tools.search]
approval = "auto"

[profiles.local]
provider = "openai-chat"
model = "mcp-model"
api_key_env = "TARZ_CONFIG_TEST_KEY"
workspace_root = "{}"
tools = ["read_file"]
mcp = ["git"]
"#,
        workspace.display()
    )
}

fn session_record(dir: &Path, id: &str) -> (std::path::PathBuf, Value) {
    let path = dir.join("store/sessions").join(format!("{id}.json"));
    (
        path.clone(),
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn wm10_and_wm11_mcp_snapshot_selection_and_credential_reference_survive_save_list_and_reopen()
 {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();
    let secret = "mcp-secret-value-6f1c";
    let token_env = "DOCS_MCP_TOKEN";
    let create = |extra: &[&str]| {
        let mut command = cli(dir.path());
        command
            .env(token_env, secret)
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .arg("--json");
        command.args(extra);
        json(command.output().unwrap())
    };

    // Preset selection freezes the git server only.
    let preset = create(&[]);
    let preset_id = preset["conversation_id"].as_str().unwrap();
    let (preset_record, preset_json) = session_record(dir.path(), preset_id);
    let servers = preset_json["definition"]["mcp_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    let git = &servers[0];
    assert_eq!(git["id"], "git");
    assert_eq!(git["command"], "uvx");
    assert_eq!(
        git["args"],
        serde_json::json!(["mcp-server-git", "--repository", "."])
    );
    assert_eq!(git["env"], serde_json::json!({"LOG_LEVEL": "warn"}));
    assert_eq!(git["env_vars"], serde_json::json!(["GIT_AUTHOR_NAME"]));
    assert_eq!(git["startup_timeout_sec"], 12);
    assert_eq!(git["tool_timeout_sec"], 120);
    assert_eq!(
        git["enabled_tools"],
        serde_json::json!(["git_status", "git_diff"])
    );
    assert_eq!(git["approval"], "writes");
    assert_eq!(git["tools"]["git_status"]["approval"], "auto");
    assert_eq!(git["required"], true);
    assert!(git.get("url").is_none());
    assert!(git.get("bearer_token_env_var").is_none());
    let preset_bytes = std::fs::read_to_string(&preset_record).unwrap();
    assert!(!preset_bytes.contains(secret));
    assert!(!preset_bytes.contains(token_env));

    // Explicit --mcp replaces the preset and keeps only variable names.
    let explicit = create(&["--mcp", "git,docs"]);
    let explicit_id = explicit["conversation_id"].as_str().unwrap();
    let (explicit_record, explicit_json) = session_record(dir.path(), explicit_id);
    let servers = explicit_json["definition"]["mcp_servers"]
        .as_array()
        .unwrap();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0]["id"], "git");
    assert_eq!(servers[1]["id"], "docs");
    assert_eq!(servers[1]["url"], "https://example.invalid/mcp");
    assert_eq!(servers[1]["bearer_token_env_var"], token_env);
    assert_eq!(servers[1]["startup_timeout_sec"], 10);
    assert_eq!(servers[1]["tool_timeout_sec"], 60);
    assert_eq!(servers[1]["approval"], "prompt");
    assert_eq!(servers[1]["required"], false);
    assert_eq!(servers[1]["disabled_tools"], serde_json::json!(["danger"]));
    assert_eq!(servers[1]["tools"]["search"]["approval"], "auto");
    assert!(servers[1].get("enabled_tools").is_none());
    let explicit_bytes = std::fs::read_to_string(&explicit_record).unwrap();
    assert!(explicit_bytes.contains(token_env));
    assert!(!explicit_bytes.contains(secret));

    // Listing and reopening read the saved definition without the secret.
    let listing = cli(dir.path())
        .env(token_env, secret)
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(listing.status.success(), "{listing:?}");
    assert!(!String::from_utf8_lossy(&listing.stdout).contains(secret));
    let listing: Value = serde_json::from_slice(&listing.stdout).unwrap();
    let listed = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["conversation_id"] == explicit_id)
        .unwrap();
    assert_eq!(
        listed["definition"]["mcp_servers"].as_array().unwrap(),
        servers
    );
    assert_eq!(listed["definition"]["permission_mode"], Value::Null);

    let repair = cli(dir.path())
        .env(token_env, secret)
        .args(["session", "repair", explicit_id, "--config"])
        .arg(&config_path)
        .args(["--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(repair.status.success(), "{repair:?}");
    let repair_json: Value = serde_json::from_slice(&repair.stdout).unwrap();
    assert_eq!(repair_json["status"], "not_needed");
    assert!(!String::from_utf8_lossy(&repair.stdout).contains(secret));

    let host = tarz_core::application::SessionHost::new(Some(dir.path().join("store")));
    let entries = host.list().await.unwrap();
    let reopened = entries
        .iter()
        .find(|entry| entry.conversation_id.as_str() == explicit_id)
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert_eq!(reopened.definition.mcp_servers.len(), 2);
    assert_eq!(reopened.definition.mcp_servers[1].id, "docs");
    assert_eq!(
        reopened.definition.mcp_servers[1]
            .bearer_token_env_var
            .as_deref(),
        Some(token_env)
    );

    // An explicit empty selection clears the preset's server.
    let cleared = create(&["--mcp", ""]);
    let cleared_id = cleared["conversation_id"].as_str().unwrap();
    let (cleared_record, cleared_json) = session_record(dir.path(), cleared_id);
    assert!(cleared_json["definition"].get("mcp_servers").is_none());
    assert!(
        !std::fs::read_to_string(cleared_record)
            .unwrap()
            .contains("mcp_servers")
    );

    // Materials export contains no definition or credential value.
    let exported = dir.path().join("exported.json");
    let export = cli(dir.path())
        .env(token_env, secret)
        .args(["session", "export-materials", "--record"])
        .arg(&explicit_record)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");
    assert!(!std::fs::read_to_string(&exported).unwrap().contains(secret));
    assert!(!String::from_utf8_lossy(&export.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&export.stderr).contains(secret));
}

#[tokio::test]
async fn wm10_legacy_v2_record_without_mcp_servers_reads_as_empty_and_ask() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let sessions = store.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let id = "018f0c4e-0000-7000-8000-000000000001";
    let commit = "018f0c4e-0000-7000-8000-000000000002";
    let record_path = sessions.join(format!("{id}.json"));
    // A fixed record written before MCP snapshots and initial modes existed.
    let raw = format!(
        r#"{{"format":"tarz-session-v2","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&record_path, raw.as_bytes()).unwrap();
    let original = std::fs::read(&record_path).unwrap();

    let listing = json(
        cli(dir.path())
            .args(["session", "list", "--no-config", "--store"])
            .arg(&store)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let listed = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["conversation_id"] == id)
        .unwrap();
    assert_eq!(listed["definition"]["model"], "legacy-model");
    assert!(listed["definition"].get("mcp_servers").is_none());
    assert!(listed["definition"].get("permission_mode").is_none());

    // Opening through repair must accept the legacy record and leave it untouched.
    let repair = cli(dir.path())
        .args(["session", "repair", id, "--no-config", "--store"])
        .arg(&store)
        .args(["--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(repair.status.success(), "{repair:?}");
    let opened: Value = serde_json::from_slice(&repair.stdout).unwrap();
    assert_eq!(opened["status"], "not_needed");
    assert_eq!(std::fs::read(&record_path).unwrap(), original);

    let entries = tarz_core::application::SessionHost::new(Some(store))
        .list()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let definition = &entries[0].record.as_ref().unwrap().definition;
    assert!(definition.mcp_servers.is_empty());
    assert_eq!(
        definition.initial_permission_mode(),
        tarz_core::permission::PermissionMode::Ask
    );
}

#[test]
fn wm11_cli_rejects_unknown_mcp_ids_and_keeps_secrets_out_of_errors() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();
    let secret = "mcp-secret-error-value";

    let run_unknown = cli(dir.path())
        .env("DOCS_MCP_TOKEN", secret)
        .args(["run", "--config"])
        .arg(&config_path)
        .args(["--mcp", "absent", "--input", "hello"])
        .output()
        .unwrap();
    assert_eq!(run_unknown.status.code(), Some(2), "{run_unknown:?}");
    assert!(!String::from_utf8_lossy(&run_unknown.stderr).contains(secret));

    for args in [
        vec!["--mcp", "absent"],
        vec!["--mcp", "git,git"],
        vec!["--mcp", "Git"],
    ] {
        let output = cli(dir.path())
            .env("DOCS_MCP_TOKEN", secret)
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains(secret), "{error}");
    }

    // A malformed, unselected server fails at load without echoing values.
    let broken = format!(
        "{}\n[mcp_servers.broken]\ncommand = \"uvx\"\nenv = {{ \"BAD-NAME\" = \"secret-marker\" }}\n",
        mcp_config(&workspace)
    );
    std::fs::write(&config_path, broken).unwrap();
    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("secret-marker"), "{error}");

    // --mcp needs a value and is not accepted when reopening a session.
    let output = cli(dir.path())
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .arg("--mcp")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("option needs a value"));
    let output = cli(dir.path())
        .args([
            "session",
            "run",
            "00000000-0000-0000-0000-000000000000",
            "--mcp",
            "git",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown, duplicate, or inapplicable")
    );
}

#[test]
fn wm11_mcp_selection_is_documented_in_cli_help() {
    let dir = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--help"],
        vec!["run", "--help"],
        vec!["session", "--help"],
        vec!["session", "create", "--help"],
    ] {
        let output = cli(dir.path()).args(&arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(
            help.contains("--mcp") && help.contains("mcp_servers"),
            "{arguments:?}"
        );
        assert!(
            help.contains("default: profile; empty disables"),
            "{arguments:?}"
        );
    }
}
