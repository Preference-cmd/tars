//! Shared TOML is exercised through actual CLI processes, without live models.
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn config() -> &'static str {
    "version = 1\nstore = 'configured-data'\nrecovery_dir = '.'\ndefault_profile = 'first'\n[profiles.first]\nprovider = 'openai-chat'\nmodel = 'saved-model'\napi_key_env = 'TARS_CONFIG_TEST_KEY'\ntimeout_secs = 17\n[profiles.second]\nprovider = 'anthropic'\nmodel = 'second-model'\napi_key_env = 'TARS_CONFIG_TEST_KEY'\n"
}
fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env("TARS_HOME", home.join("tars-home"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TARS_CONFIG_TEST_KEY", "fixture-only");
    command
}
fn json(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}
fn read_db_record(store: &Path, id: &str) -> Value {
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    struct DbRow {
        revision: i64,
        last_commit_id: String,
        policy_json: String,
        title: Option<String>,
        permission_mode: Option<String>,
        auto_compact: Option<i64>,
        compact_threshold: Option<i64>,
        usage_json: Option<String>,
        updated_at_ms: i64,
        def_json: String,
        ctx_json: String,
        history_json: String,
    }
    let DbRow {
        revision,
        last_commit_id,
        policy_json,
        title,
        permission_mode,
        auto_compact,
        compact_threshold,
        usage_json,
        updated_at_ms,
        def_json,
        ctx_json,
        history_json,
    } = conn
        .query_row(
            "SELECT
                s.revision, s.last_commit_id, s.policy_json, s.title, s.permission_mode,
                s.auto_compact, s.compact_threshold, s.usage_json, s.updated_at_ms,
                a.definition_json, a.context_json, a.history_index_json
             FROM sessions s
             JOIN agents a ON s.session_id = a.session_id AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            rusqlite::params![id],
            |row| {
                Ok(DbRow {
                    revision: row.get(0)?,
                    last_commit_id: row.get(1)?,
                    policy_json: row.get(2)?,
                    title: row.get(3)?,
                    permission_mode: row.get(4)?,
                    auto_compact: row.get(5)?,
                    compact_threshold: row.get(6)?,
                    usage_json: row.get(7)?,
                    updated_at_ms: row.get(8)?,
                    def_json: row.get(9)?,
                    ctx_json: row.get(10)?,
                    history_json: row.get(11)?,
                })
            },
        )
        .unwrap();
    let policy: Value = serde_json::from_str(&policy_json).unwrap();
    let mut obj = serde_json::Map::new();
    obj.insert("format".into(), policy["format"].clone());
    obj.insert("conversation_id".into(), serde_json::json!(id));
    obj.insert("revision".into(), serde_json::json!(revision));
    obj.insert("last_commit_id".into(), serde_json::json!(last_commit_id));
    obj.insert(
        "definition".into(),
        serde_json::from_str(&def_json).unwrap(),
    );
    obj.insert("context".into(), serde_json::from_str(&ctx_json).unwrap());
    if let Some(t) = title {
        obj.insert("title".into(), serde_json::json!(t));
    }
    obj.insert("updated_at_ms".into(), serde_json::json!(updated_at_ms));
    if let Some(m) = permission_mode {
        obj.insert("permission_mode".into(), serde_json::json!(m));
    }
    if let Some(a) = auto_compact {
        obj.insert("auto_compact".into(), serde_json::json!(a != 0));
    }
    if let Some(ct) = compact_threshold {
        obj.insert("compact_threshold".into(), serde_json::json!(ct));
    }
    let history: Vec<Value> = serde_json::from_str(&history_json).unwrap();
    if !history.is_empty() {
        obj.insert("history".into(), Value::Array(history));
    }
    if let Some(u) = usage_json {
        obj.insert("usage".into(), serde_json::from_str(&u).unwrap());
    }
    Value::Object(obj)
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
    let configured_store = dir.path().join("configured-data");
    let before = serde_json::to_string(&read_db_record(&configured_store, id)).unwrap();
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
    assert_eq!(
        serde_json::to_string(&read_db_record(&configured_store, id)).unwrap(),
        before
    );
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
    let global = dir.path().join("tars-home");
    std::fs::create_dir(&global).unwrap();
    std::fs::write(global.join("config.toml"), config()).unwrap();
    let default = json(
        cli(dir.path())
            .args(["session", "create", "--json"])
            .output()
            .unwrap(),
    );
    assert!(global.join("configured-data/tars.db").exists());
    let _ = read_db_record(
        &global.join("configured-data"),
        default["conversation_id"].as_str().unwrap(),
    );
    let fallback_home = dir.path().join("fallback-home");
    let dot_tars = fallback_home.join(".tars");
    std::fs::create_dir_all(&dot_tars).unwrap();
    std::fs::write(dot_tars.join("config.toml"), config()).unwrap();
    let fallback = json(
        cli(dir.path())
            .env("TARS_HOME", "relative-ignored")
            .env("HOME", &fallback_home)
            .env("USERPROFILE", &fallback_home)
            .args(["session", "create", "--json"])
            .output()
            .unwrap(),
    );
    assert!(dot_tars.join("configured-data/tars.db").exists());
    let _ = read_db_record(
        &dot_tars.join("configured-data"),
        fallback["conversation_id"].as_str().unwrap(),
    );
    let env_path = dir.path().join("environment.toml");
    std::fs::write(&env_path, config().replace("saved-model", "env-model")).unwrap();
    let environment = json(
        cli(dir.path())
            .env("TARS_CONFIG", &env_path)
            .args(["session", "create", "--json"])
            .output()
            .unwrap(),
    );
    let record = read_db_record(
        &dir.path().join("configured-data"),
        environment["conversation_id"].as_str().unwrap(),
    );
    assert_eq!(record["definition"]["model"], "env-model");
    let missing = dir.path().join("missing.toml");
    let output = cli(dir.path())
        .env("TARS_CONFIG", missing)
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
    assert_eq!(output.status.code(), Some(1));
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
    let frozen = read_db_record(
        &dir.path().join("configured-data"),
        created["conversation_id"].as_str().unwrap(),
    );
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
    assert_eq!(
        read_db_record(
            &dir.path().join("configured-data"),
            created["conversation_id"].as_str().unwrap(),
        ),
        frozen
    );
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
                "TARS_CONFIG_TEST_KEY",
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
    let context = causa::kernel::Context::from_blocks(vec![tars_core::application::text_input(
        "legacy material retained",
    )])
    .unwrap();
    tars_core::material::save_new(&material, &context).unwrap();
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
                "TARS_CONFIG_TEST_KEY",
                "--load",
            ])
            .arg(&material)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();
    let mut value = read_db_record(&store, id);
    value["format"] = serde_json::json!("tars-session-v1");
    for key in ["workspace_root", "tools", "tool_limits"] {
        value["definition"].as_object_mut().unwrap().remove(key);
    }
    let record = dir.path().join(format!("{id}-v1.json"));
    std::fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    let original = std::fs::read(&record).unwrap();
    let import_store = dir.path().join("import-store");
    let imported = cli(dir.path())
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&record)
        .arg("--store")
        .arg(&import_store)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(imported.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&imported.stdout).contains("unsupported session format"));
    let exported = dir.path().join("export.json");
    let result = cli(dir.path())
        .env("TARS_CONFIG", dir.path().join("missing.toml"))
        .env_remove("TARS_CONFIG_TEST_KEY")
        .args(["session", "export-materials", "--record"])
        .arg(&record)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let exported_context = tars_core::material::load(&exported).unwrap();
    assert!(tars_core::prelude::has_prelude(exported_context.blocks()));
    assert_eq!(
        &exported_context.blocks()[exported_context.blocks().len() - context.blocks().len()..],
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

#[test]
fn v2_record_is_refused_with_the_export_path_and_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let material = dir.path().join("initial.json");
    let context = causa::kernel::Context::from_blocks(vec![tars_core::application::text_input(
        "v2 material retained",
    )])
    .unwrap();
    tars_core::material::save_new(&material, &context).unwrap();
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
                "TARS_CONFIG_TEST_KEY",
                "--load",
            ])
            .arg(&material)
            .arg("--json")
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();
    let mut value = read_db_record(&store, id);
    assert_eq!(value["format"], "tars-session-v2.4");
    value["format"] = serde_json::json!("tars-session-v2");
    let record = dir.path().join(format!("{id}-v2.json"));
    std::fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    let original = std::fs::read(&record).unwrap();
    let import_store = dir.path().join("import-store");
    let imported = cli(dir.path())
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&record)
        .arg("--store")
        .arg(&import_store)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(imported.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&imported.stdout);
    assert!(
        stdout.contains("unsupported session format: tars-session-v2;"),
        "{stdout}"
    );
    assert!(stdout.contains("expected tars-session-v2.4"), "{stdout}");
    assert!(stdout.contains("tars session export-materials"), "{stdout}");
    assert_eq!(std::fs::read(&record).unwrap(), original);
    let exported = dir.path().join("export.json");
    let result = cli(dir.path())
        .env("TARS_CONFIG", dir.path().join("missing.toml"))
        .env_remove("TARS_CONFIG_TEST_KEY")
        .args(["session", "export-materials", "--record"])
        .arg(&record)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let exported_context = tars_core::material::load(&exported).unwrap();
    assert!(tars_core::prelude::has_prelude(exported_context.blocks()));
    assert_eq!(
        &exported_context.blocks()[exported_context.blocks().len() - context.blocks().len()..],
        context.blocks()
    );
    assert_eq!(std::fs::read(&record).unwrap(), original);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn wm10_definition_save_and_reopen_and_export_omits_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let ws_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let secret_env = "TARS_WM10_SECRET";
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
        toml_path(&ws_dir)
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

    let stored = read_db_record(&dir.path().join("store"), id);
    let record_str = serde_json::to_string(&stored).unwrap();

    assert!(
        !record_str.contains(secret_val),
        "record bytes must not contain credential value"
    );
    assert!(
        record_str.contains(secret_env),
        "record must contain credential variable name reference"
    );

    let snapshot_record = dir.path().join(format!("{id}-snapshot.json"));
    std::fs::write(&snapshot_record, serde_json::to_vec(&stored).unwrap()).unwrap();
    let exported = dir.path().join("exported.json");
    let export_res = cli(dir.path())
        .env(secret_env, secret_val)
        .args(["session", "export-materials", "--record"])
        .arg(&snapshot_record)
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

    let host = tars_core::application::SessionHost::new(Some(dir.path().join("store")));
    let host_list = host.list().await.unwrap();
    assert_eq!(host_list.len(), 1);
    let record_summary = host_list[0].record.as_ref().unwrap();
    assert_eq!(
        record_summary.definition.tools,
        vec![
            tars_core::definition::WorkspaceTool::ReadFile,
            tars_core::definition::WorkspaceTool::WriteFile,
            tars_core::definition::WorkspaceTool::EditFile,
        ]
    );
    assert_eq!(
        record_summary.definition.permission_mode,
        Some(tars_core::permission::PermissionMode::AcceptEdits)
    );
    assert_eq!(
        record_summary.definition.write_limits,
        Some(tars_core::definition::WriteToolLimits {
            max_write_bytes: 524288,
            max_edit_file_bytes: 1048576,
            max_edit_segments: 16,
        })
    );
}

/// TOML basic strings treat backslashes as escapes, so Windows paths must be
/// doubled before they are interpolated into a quoted value.
fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

/// MCP-focused profile: a workspace root without workspace tools, so session
/// creation does not need the Unix-only workspace backend.
fn mcp_config(workspace: &Path) -> String {
    format!(
        r#"version = 1
store = "store"
recovery_dir = "."
default_profile = "local"

[profiles.local]
provider = "openai-chat"
model = "mcp-model"
api_key_env = "TARS_CONFIG_TEST_KEY"
workspace_root = "{}"
mcp = ["git"]
"#,
        toml_path(workspace)
    )
}

fn mcp_json() -> &'static str {
    r#"{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "args": ["mcp-server-git", "--repository", "."],
      "env": { "LOG_LEVEL": "warn", "GIT_AUTHOR_NAME": "${GIT_AUTHOR_NAME}" },
      "startupTimeoutSec": 12,
      "toolTimeoutSec": 120,
      "enabledTools": ["git_status", "git_diff"],
      "approval": "writes",
      "required": true,
      "tools": { "git_status": { "approval": "approve" } }
    },
    "docs": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_MCP_TOKEN}" },
      "disabledTools": ["danger"],
      "tools": { "search": { "approval": "approve" } }
    }
  }
}"#
}

fn session_record(dir: &Path, id: &str) -> Value {
    read_db_record(&dir.join("store"), id)
}

#[tokio::test]
async fn wm10_and_wm11_mcp_snapshot_selection_and_credential_reference_survive_save_list_and_reopen()
 {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();
    std::fs::write(dir.path().join("mcp.json"), mcp_json()).unwrap();
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
    let preset_json = session_record(dir.path(), preset_id);
    let servers = preset_json["definition"]["mcp_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    let git = &servers[0];
    assert_eq!(git["id"], "git");
    assert_eq!(git["command"], "uvx");
    assert_eq!(
        git["args"],
        serde_json::json!(["mcp-server-git", "--repository", "."])
    );
    assert_eq!(
        git["env"],
        serde_json::json!({"LOG_LEVEL": "warn", "GIT_AUTHOR_NAME": "${GIT_AUTHOR_NAME}"})
    );
    assert_eq!(git["startup_timeout_sec"], 12);
    assert_eq!(git["tool_timeout_sec"], 120);
    assert_eq!(
        git["enabled_tools"],
        serde_json::json!(["git_status", "git_diff"])
    );
    assert_eq!(git["approval"], "writes");
    assert_eq!(git["tools"]["git_status"]["approval"], "approve");
    assert_eq!(git["required"], true);
    assert!(git.get("url").is_none());
    assert!(git.get("headers").is_none());
    assert!(git.get("env_vars").is_none());
    assert!(git.get("bearer_token_env_var").is_none());
    let preset_bytes = serde_json::to_string(&preset_json).unwrap();
    assert!(!preset_bytes.contains(secret));
    assert!(!preset_bytes.contains(token_env));

    // Explicit --mcp replaces the preset and keeps only variable names.
    let explicit = create(&["--mcp", "git,docs"]);
    let explicit_id = explicit["conversation_id"].as_str().unwrap();
    let explicit_json = session_record(dir.path(), explicit_id);
    let servers = explicit_json["definition"]["mcp_servers"]
        .as_array()
        .unwrap();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0]["id"], "git");
    assert_eq!(servers[1]["id"], "docs");
    assert_eq!(servers[1]["url"], "https://example.invalid/mcp");
    assert_eq!(
        servers[1]["headers"],
        serde_json::json!({ "Authorization": "Bearer ${DOCS_MCP_TOKEN}" })
    );
    assert_eq!(servers[1]["startup_timeout_sec"], 10);
    assert_eq!(servers[1]["tool_timeout_sec"], 60);
    assert_eq!(servers[1]["approval"], "prompt");
    assert_eq!(servers[1]["required"], false);
    assert_eq!(servers[1]["disabled_tools"], serde_json::json!(["danger"]));
    assert_eq!(servers[1]["tools"]["search"]["approval"], "approve");
    assert!(servers[1].get("enabled_tools").is_none());
    assert!(servers[1].get("bearer_token_env_var").is_none());
    let explicit_bytes = serde_json::to_string(&explicit_json).unwrap();
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

    let host = tars_core::application::SessionHost::new(Some(dir.path().join("store")));
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
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer ${DOCS_MCP_TOKEN}")
    );

    // An explicit empty selection clears the preset's server.
    let cleared = create(&["--mcp", ""]);

    // Reopening and listing must still work from the saved definition.
    std::fs::remove_file(dir.path().join("mcp.json")).unwrap();
    let entries_after_delete = host.list().await.unwrap();
    assert_eq!(entries_after_delete.len(), 3);
    let cleared_id = cleared["conversation_id"].as_str().unwrap();
    let cleared_json = session_record(dir.path(), cleared_id);
    assert!(cleared_json["definition"].get("mcp_servers").is_none());
    assert!(
        !serde_json::to_string(&cleared_json)
            .unwrap()
            .contains("mcp_servers")
    );

    // Materials export contains no definition or credential value.
    let explicit_snapshot = dir.path().join("explicit-snapshot.json");
    std::fs::write(
        &explicit_snapshot,
        serde_json::to_vec(&explicit_json).unwrap(),
    )
    .unwrap();
    let exported = dir.path().join("exported.json");
    let export = cli(dir.path())
        .env(token_env, secret)
        .args(["session", "export-materials", "--record"])
        .arg(&explicit_snapshot)
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
async fn wm10_record_without_mcp_servers_reads_as_empty_and_ask() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let legacy_dir = dir.path().join("legacy");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let id = "018f0c4e-0000-7000-8000-000000000001";
    let commit = "018f0c4e-0000-7000-8000-000000000002";
    let record_path = legacy_dir.join(format!("{id}.json"));
    // A fixed record written before MCP snapshots and initial modes existed.
    let raw = format!(
        r#"{{"format":"tars-session-v2.4","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","updated_at_ms":1000,"definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&record_path, raw.as_bytes()).unwrap();
    let original = std::fs::read(&record_path).unwrap();

    let import_out = cli(dir.path())
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&record_path)
        .arg("--store")
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(import_out.status.success(), "{import_out:?}");
    assert_eq!(std::fs::read(&record_path).unwrap(), original);

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

    let entries = tars_core::application::SessionHost::new(Some(store))
        .list()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let definition = &entries[0].record.as_ref().unwrap().definition;
    assert!(definition.mcp_servers.is_empty());
    assert_eq!(
        definition.initial_permission_mode(),
        tars_core::permission::PermissionMode::Ask
    );
}

#[test]
fn wm11_cli_rejects_unknown_mcp_ids_and_keeps_secrets_out_of_errors() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();
    std::fs::write(dir.path().join("mcp.json"), mcp_json()).unwrap();
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
    std::fs::write(
        dir.path().join("mcp.json"),
        r#"{"mcpServers": {"broken": {"command": "uvx", "env": {"BAD-NAME": "secret-marker"}}}}"#,
    )
    .unwrap();
    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("secret-marker"), "{error}");

    std::fs::write(
        &config_path,
        format!(
            "{}\n[mcp_servers.broken]\ncommand = \"uvx\"\n",
            mcp_config(&workspace)
        ),
    )
    .unwrap();
    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("mcp.json"), "{error}");

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
            help.contains("--mcp") && help.contains("mcp.json"),
            "{arguments:?}"
        );
        assert!(
            help.contains("default: profile; empty disables"),
            "{arguments:?}"
        );
    }
}

#[tokio::test]
async fn wm10_stage_g_snapshot_conversion_to_current_format() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let legacy_dir = dir.path().join("legacy");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let id = "018f0c4e-0000-7000-8000-000000000009";
    let commit = "018f0c4e-0000-7000-8000-00000000000a";
    let record_path = legacy_dir.join(format!("{id}.json"));
    let raw = format!(
        r#"{{"format":"tars-session-v2.4","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","updated_at_ms":1000,"definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"mcp_servers":[{{"id":"git","command":"uvx","env":{{"LOG_LEVEL":"warn"}},"env_vars":["GIT_AUTHOR_NAME"],"startup_timeout_sec":12,"tool_timeout_sec":120,"approval":"auto","tools":{{"git_status":{{"approval":"auto"}}}},"required":false}},{{"id":"docs","url":"https://example.invalid/mcp","bearer_token_env_var":"DOCS_MCP_TOKEN","startup_timeout_sec":10,"tool_timeout_sec":60,"approval":"prompt","required":false}}],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&record_path, raw.as_bytes()).unwrap();

    let import_out = cli(dir.path())
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&record_path)
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(import_out.status.success(), "{import_out:?}");

    let host = tars_core::application::SessionHost::new(Some(store));
    let entries = host.list().await.unwrap();
    assert_eq!(entries.len(), 1);
    let def = &entries[0].record.as_ref().unwrap().definition;
    assert_eq!(def.mcp_servers.len(), 2);
    let git = &def.mcp_servers[0];
    assert_eq!(git.env["GIT_AUTHOR_NAME"], "${GIT_AUTHOR_NAME}");
    assert_eq!(git.approval, tars_core::definition::McpApproval::Approve);
    assert_eq!(
        git.tools["git_status"].approval,
        tars_core::definition::McpApproval::Approve
    );

    let docs = &def.mcp_servers[1];
    assert_eq!(docs.headers["Authorization"], "Bearer ${DOCS_MCP_TOKEN}");
    assert_eq!(docs.approval, tars_core::definition::McpApproval::Prompt);

    let serialized = serde_json::to_value(def).unwrap();
    let s_git = &serialized["mcp_servers"][0];
    assert!(s_git.get("env_vars").is_none());
    assert_eq!(s_git["approval"], "approve");
    assert_eq!(s_git["tools"]["git_status"]["approval"], "approve");
    let s_docs = &serialized["mcp_servers"][1];
    assert!(s_docs.get("bearer_token_env_var").is_none());
    assert_eq!(
        s_docs["headers"]["Authorization"],
        "Bearer ${DOCS_MCP_TOKEN}"
    );
}

#[test]
fn wm11_cli_prints_ignored_keys_warning_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();
    std::fs::write(
        dir.path().join("mcp.json"),
        r#"{"mcpServers": {"git": {"command": "uvx", "disabled": true, "autoApprove": ["status"]}}}"#,
    )
    .unwrap();

    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git") && stderr.contains("disabled"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("git") && stderr.contains("autoApprove"),
        "stderr: {stderr}"
    );
}

#[test]
fn wm11_cli_mcp_config_location_rules() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let custom_json =
        r#"{"mcpServers": {"git": {"command": "uvx"}, "custom": {"command": "uvx"}}}"#;

    // Explicit TARS_MCP_CONFIG file path is loaded.
    let mcp_custom_path = dir.path().join("custom_mcp.json");
    std::fs::write(&mcp_custom_path, custom_json).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();

    let out1 = cli(dir.path())
        .env("TARS_MCP_CONFIG", &mcp_custom_path)
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .args(["--mcp", "custom", "--json"])
        .output()
        .unwrap();
    assert!(out1.status.success(), "case 1: {out1:?}");
    let val1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(val1["status"], "created");

    // Explicit TARS_MCP_CONFIG pointing to non-existent file fails.
    let out2 = cli(dir.path())
        .env("TARS_MCP_CONFIG", dir.path().join("non_existent_mcp.json"))
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .args(["--mcp", "custom"])
        .output()
        .unwrap();
    assert_eq!(out2.status.code(), Some(1), "case 2: {out2:?}");

    // Sibling mcp.json in the same directory as TARS_CONFIG is loaded.
    let dir3 = tempfile::tempdir().unwrap();
    let ws3 = dir3.path().join("workspace");
    std::fs::create_dir(&ws3).unwrap();
    let cfg3 = dir3.path().join("config.toml");
    std::fs::write(&cfg3, mcp_config(&ws3)).unwrap();
    std::fs::write(dir3.path().join("mcp.json"), custom_json).unwrap();

    let out3 = cli(dir.path())
        .env("TARS_CONFIG", &cfg3)
        .env_remove("TARS_MCP_CONFIG")
        .args(["session", "create", "--mcp", "custom", "--json"])
        .output()
        .unwrap();
    assert!(out3.status.success(), "case 3: {out3:?}");

    // Sibling mcp.json in the same directory as --config is loaded.
    let dir4 = tempfile::tempdir().unwrap();
    let ws4 = dir4.path().join("workspace");
    std::fs::create_dir(&ws4).unwrap();
    let cfg4 = dir4.path().join("config.toml");
    std::fs::write(&cfg4, mcp_config(&ws4)).unwrap();
    std::fs::write(dir4.path().join("mcp.json"), custom_json).unwrap();

    let out4 = cli(dir.path())
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_MCP_CONFIG")
        .args(["session", "create", "--config"])
        .arg(&cfg4)
        .args(["--mcp", "custom", "--json"])
        .output()
        .unwrap();
    assert!(out4.status.success(), "case 4: {out4:?}");

    // Disabled configuration never reads mcp.json even when TARS_MCP_CONFIG is set.
    let out5 = cli(dir.path())
        .env("TARS_MCP_CONFIG", &mcp_custom_path)
        .args(["session", "create", "--no-config", "--mcp", "custom"])
        .output()
        .unwrap();
    assert_eq!(out5.status.code(), Some(2), "case 5: {out5:?}");

    // When no default config path exists (no TARS_HOME, HOME, or USERPROFILE),
    // cwd mcp.json is NOT loaded (regression test).
    let dir6 = tempfile::tempdir().unwrap();
    std::fs::write(dir6.path().join("mcp.json"), custom_json).unwrap();
    let out6 = cli(dir.path())
        .env_remove("TARS_HOME")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_MCP_CONFIG")
        .current_dir(dir6.path())
        .args(["session", "create", "--mcp", "custom"])
        .output()
        .unwrap();
    assert_eq!(out6.status.code(), Some(2), "case 6: {out6:?}");
}

#[test]
fn wm11_cli_negative_config_does_not_spawn_child_processes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, mcp_config(&workspace)).unwrap();

    let marker_create = dir.path().join("create_spawned.marker");
    let marker_run = dir.path().join("run_spawned.marker");
    let secret = "test_forbidden_secret_value_999";

    let mcp_content_create = format!(
        r#"{{"mcpServers": {{"evil": {{"command": "/bin/sh", "args": ["-c", "touch {}"], "headers": {{"Authorization": "{}"}}}}}}}}"#,
        marker_create.display(),
        secret
    );
    std::fs::write(dir.path().join("mcp.json"), mcp_content_create).unwrap();

    let out_create = cli(dir.path())
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .args(["--mcp", "evil"])
        .output()
        .unwrap();
    assert_eq!(out_create.status.code(), Some(1));
    assert!(
        !marker_create.exists(),
        "process was spawned during session create!"
    );
    let stderr_create = String::from_utf8_lossy(&out_create.stderr);
    assert!(
        !stderr_create.contains(secret),
        "stderr leaked secret: {stderr_create}"
    );

    let mcp_content_run = format!(
        r#"{{"mcpServers": {{"evil": {{"command": "/bin/sh", "args": ["-c", "touch {}"], "headers": {{"Authorization": "{}"}}}}}}}}"#,
        marker_run.display(),
        secret
    );
    std::fs::write(dir.path().join("mcp.json"), mcp_content_run).unwrap();

    let out_run = cli(dir.path())
        .args(["run", "--config"])
        .arg(&config_path)
        .args(["--mcp", "evil", "--input", "hello"])
        .output()
        .unwrap();
    assert_eq!(out_run.status.code(), Some(1));
    assert!(!marker_run.exists(), "process was spawned during run!");
    let stderr_run = String::from_utf8_lossy(&out_run.stderr);
    assert!(
        !stderr_run.contains(secret),
        "stderr leaked secret: {stderr_run}"
    );
}

#[test]
fn exit_codes_separate_configuration_errors_from_argument_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config()).unwrap();

    let invalid = dir.path().join("invalid.toml");
    std::fs::write(&invalid, "version = 1\nunknown_key = true").unwrap();
    let output = cli(dir.path())
        .args(["session", "list", "--config"])
        .arg(&invalid)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid TOML"));

    let output = cli(dir.path())
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .args(["--profile", "absent"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown configuration profile"));

    for arguments in [
        vec!["session", "list", "--bogus"],
        vec!["session", "list", "--store"],
    ] {
        let output = cli(dir.path()).args(&arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
    }

    let output = cli(dir.path())
        .args(["session", "create", "--config"])
        .arg(&config_path)
        .args(["--tools", "list_directory,bogus"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    for tool in tars_core::definition::WorkspaceTool::ALL {
        assert!(error.contains(tool.name()), "{tool:?} missing: {error}");
    }
}

#[test]
fn invalid_values_inside_a_config_profile_remain_configuration_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "version = 1\ndefault_profile = 'p'\n[profiles.p]\nprovider = 'openai-chat'\nmodel = 'm'\ntimeout_secs = 0\n",
    )
    .unwrap();
    let output = cli(dir.path())
        .args(["session", "create", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("timeout"));
}

#[test]
fn profile_definition_errors_exit_as_configuration_errors() {
    // Config::load resolves every profile before a command can override it, so
    // both a definition-free (list) and a building command (create) must report
    // a configuration exit code and no argument hint.
    let cases = [
        ("model = '   '", "model identifier must not be blank"),
        (
            "model = 'm'\napi_key_env = '1BAD'",
            "credential reference must name a valid environment variable",
        ),
        (
            "model = 'm'\nendpoint = 'ftp://example.invalid/v1'",
            "endpoint must be an absolute HTTP(S) URL",
        ),
        ("model = 'm'\nmax_tokens = 0", "max_tokens must be positive"),
        (
            "model = 'm'\nmax_model_attempts = 0",
            "max_model_attempts must be positive",
        ),
        (
            "model = 'm'\n[profiles.p.render_options]\nmax_tokens_field = 'secret_marker'",
            "render_options has a key or value",
        ),
        (
            "model = 'm'\nworkspace_root = '.'\ntools = ['read_file', 'read_file']",
            "workspace tool selection contains a duplicate",
        ),
        (
            "model = 'm'\n[profiles.p.tool_limits]\nmax_output_bytes = 0",
            "workspace tool limits must be positive",
        ),
        ("model = 'm'\nmcp = ['absent']", "unknown MCP server"),
    ];
    for (profile, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = format!(
            "version = 1\nstore = 'data'\ndefault_profile = 'p'\n[profiles.p]\nprovider = 'openai-chat'\n{profile}\n"
        );
        std::fs::write(&path, text).unwrap();
        for command in ["list", "create"] {
            let output = cli(dir.path())
                .args(["session", command, "--config"])
                .arg(&path)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{profile}: {output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{profile}: {stderr}");
            assert!(
                !stderr.contains("Use tars session --help"),
                "a profile error is not an argument error: {stderr}"
            );
        }
    }
}

#[test]
fn explicit_definition_errors_exit_as_argument_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config()).unwrap();
    let cases: &[(&[&str], &str)] = &[
        (&["--max-tokens", "0"], "max_tokens must be positive"),
        (
            &["--max-model-attempts", "0"],
            "max_model_attempts must be positive",
        ),
        (
            &["--endpoint", "ftp://example.invalid/v1"],
            "endpoint must be an absolute HTTP(S) URL",
        ),
        (
            &["--api-key-env", "1BAD"],
            "credential reference must name a valid environment variable",
        ),
        (&["--model", "   "], "model identifier must not be blank"),
        (
            &[
                "--render-options",
                r#"{"max_tokens_field":"secret_marker"}"#,
            ],
            "render_options has a key or value",
        ),
        (&["--mcp", "absent"], "unknown MCP server"),
        (
            &["--tools", "read_file"],
            "workspace root is required when tools are enabled",
        ),
    ];
    for (args, expected) in cases {
        let output = cli(dir.path())
            .args(["session", "create", "--config"])
            .arg(&config_path)
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        assert!(
            stderr.contains("Use tars session --help"),
            "an explicit invalid value is an argument error: {stderr}"
        );
    }

    // The run entry point shares the same classifier.
    let output = cli(dir.path())
        .args(["run", "--config"])
        .arg(&config_path)
        .args(["--max-tokens", "0", "--input", "hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}
