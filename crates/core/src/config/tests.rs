use super::{Config, ConfigSource, DefinitionOverrides};
use crate::definition::{CredentialRef, DefinitionError, Provider, WriteToolLimits};
use crate::permission::PermissionMode;

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
            .expect_err("must reject")
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
    const MARKER: &str = "TARS_CONFIG_CWD_TEST_DESTINATION";
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

#[test]
fn render_options_come_from_the_profile_and_an_explicit_table_replaces_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text =
        format!("{CONFIG}[profiles.local.render_options]\nmax_tokens_field = \"max_tokens\"\n");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path)).unwrap();
    let preset = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(
        serde_json::Value::Object(preset.render_options.clone()),
        serde_json::json!({"max_tokens_field": "max_tokens"})
    );

    let mut replacement = serde_json::Map::new();
    replacement.insert("replay_reasoning_content".into(), serde_json::json!(false));
    let overridden = config
        .definition(
            None,
            DefinitionOverrides {
                render_options: Some(replacement.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(overridden.render_options, replacement);

    // The profile's Chat option does not fit another protocol.
    let switched = config.definition(
        None,
        DefinitionOverrides {
            provider: Some(Provider::Anthropic),
            ..Default::default()
        },
    );
    assert!(matches!(
        switched,
        Err(super::ConfigError::Definition(
            crate::definition::DefinitionError::InvalidRenderOptions
        ))
    ));
}

#[test]
fn a_profile_with_an_unknown_render_option_is_refused_at_load_without_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text =
        format!("{CONFIG}[profiles.local.render_options]\nmax_tokens_field = \"secret_marker\"\n");
    std::fs::write(&path, text).unwrap();
    let error = Config::load(ConfigSource::File(path)).expect_err("refused");
    assert!(!error.to_string().contains("secret_marker"));
    assert!(error.to_string().contains("render_options"));
}

#[test]
fn config_preset_sets_permission_mode_and_write_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = format!(
        "{CONFIG}permission_mode = \"accept-edits\"
[profiles.local.write_limits]
max_write_bytes = 500000
max_edit_file_bytes = 1000000
max_edit_segments = 10
"
    );
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path)).unwrap();
    let def = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(def.permission_mode, Some(PermissionMode::AcceptEdits));
    let wl = def.write_limits.unwrap();
    assert_eq!(wl.max_write_bytes, 500000);
    assert_eq!(wl.max_edit_file_bytes, 1000000);
    assert_eq!(wl.max_edit_segments, 10);
}

#[test]
fn config_explicit_overrides_permission_mode_and_write_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = format!(
        "{CONFIG}permission_mode = \"accept-edits\"
"
    );
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path)).unwrap();

    let overridden = config
        .definition(
            None,
            DefinitionOverrides {
                permission_mode: Some(PermissionMode::ReadOnly),
                write_limits: Some(WriteToolLimits {
                    max_write_bytes: 200000,
                    max_edit_file_bytes: 800000,
                    max_edit_segments: 5,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(overridden.permission_mode, Some(PermissionMode::ReadOnly));
    assert_eq!(overridden.write_limits.unwrap().max_write_bytes, 200000);

    // Explicit Ask normalizes to None
    let ask_overridden = config
        .definition(
            None,
            DefinitionOverrides {
                permission_mode: Some(PermissionMode::Ask),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(ask_overridden.permission_mode, None);
}

#[test]
fn config_unknown_permission_mode_is_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = format!(
        "{CONFIG}permission_mode = \"super-admin\"
"
    );
    std::fs::write(&path, text).unwrap();
    let error = Config::load(ConfigSource::File(path)).expect_err("must fail");
    assert!(matches!(error, super::ConfigError::Syntax { .. }));
}

#[test]
fn config_write_limits_can_only_be_lowered_not_raised() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let text_high = format!(
        "{CONFIG}[profiles.local.write_limits]
max_write_bytes = 99999999
"
    );
    std::fs::write(&path, text_high).unwrap();
    let err_high =
        Config::load(ConfigSource::File(path.clone())).expect_err("must reject high limit");
    assert!(matches!(
        err_high,
        super::ConfigError::Definition(DefinitionError::InvalidToolLimits)
    ));

    let text_zero = format!(
        "{CONFIG}[profiles.local.write_limits]
max_write_bytes = 0
"
    );
    std::fs::write(&path, text_zero).unwrap();
    let err_zero = Config::load(ConfigSource::File(path)).expect_err("must reject zero limit");
    assert!(matches!(
        err_zero,
        super::ConfigError::Definition(DefinitionError::InvalidToolLimits)
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn modifying_config_file_does_not_affect_reopened_session() {
    use crate::application::{ClosePolicy, PreparedCreate, SessionHost};
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let text1 = format!(
        "{CONFIG}permission_mode = \"accept-edits\"
"
    );
    std::fs::write(&cfg_path, &text1).unwrap();

    let config = Config::load(ConfigSource::File(cfg_path.clone())).unwrap();
    let mut def = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    def.credential = CredentialRef::Environment("PATH".into());
    assert_eq!(def.permission_mode, Some(PermissionMode::AcceptEdits));

    let host = SessionHost::new(Some(storage.path().to_owned()));
    let prepared = PreparedCreate::persistent(def, causa::kernel::Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();
    assert_eq!(
        session.permission_mode(),
        Some((PermissionMode::AcceptEdits, 0))
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let text2 = format!(
        "{CONFIG}permission_mode = \"read-only\"
"
    );
    std::fs::write(&cfg_path, &text2).unwrap();

    // Reopening uses the saved definition, not the changed configuration.
    let reopened = host.open(id).await.unwrap();
    assert_eq!(
        reopened.definition().permission_mode,
        Some(PermissionMode::AcceptEdits)
    );
    assert_eq!(
        reopened.permission_mode(),
        Some((PermissionMode::AcceptEdits, 0))
    );
}

const MCP_CONFIG: &str = r#"
version = 1
default_profile = "local"

[profiles.local]
provider = "openai-chat"
model = "local-model"
api_key_env = "FIXTURE_TOKEN"
mcp = ["git"]
"#;

const MCP_JSON: &str = r#"{
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
      "headers": { "Authorization": "Bearer ${DOCS_MCP_TOKEN}" }
    }
  }
}
"#;

fn mcp_config(dir: &std::path::Path) -> Config {
    let toml_path = dir.join("config.toml");
    let json_path = dir.join("mcp.json");
    std::fs::write(&toml_path, MCP_CONFIG).unwrap();
    std::fs::write(&json_path, MCP_JSON).unwrap();
    Config::load(ConfigSource::File(toml_path)).unwrap()
}

#[test]
fn wm11_valid_mcp_servers_load_with_defaults_and_preset_selection() {
    let dir = tempfile::tempdir().unwrap();
    let config = mcp_config(dir.path());
    let servers = config.mcp_servers();
    assert_eq!(servers.len(), 2);
    let git = &servers["git"];
    assert_eq!(git.command.as_deref(), Some("uvx"));
    assert_eq!(git.args, ["mcp-server-git", "--repository", "."]);
    assert_eq!(git.env["LOG_LEVEL"], "warn");
    assert_eq!(git.env["GIT_AUTHOR_NAME"], "${GIT_AUTHOR_NAME}");
    assert_eq!(git.startup_timeout_sec, 12);
    assert_eq!(git.tool_timeout_sec, 120);
    assert_eq!(
        git.enabled_tools.as_deref(),
        Some(&["git_status".to_owned(), "git_diff".to_owned()][..])
    );
    assert_eq!(git.approval, crate::definition::McpApproval::Writes);
    assert!(git.required);
    assert_eq!(
        git.tools["git_status"].approval,
        crate::definition::McpApproval::Approve
    );

    let docs = &servers["docs"];
    assert_eq!(docs.url.as_deref(), Some("https://example.invalid/mcp"));
    assert_eq!(docs.headers["Authorization"], "Bearer ${DOCS_MCP_TOKEN}");
    // Omitted sparse fields take the documented snapshot defaults.
    assert_eq!(docs.startup_timeout_sec, 10);
    assert_eq!(docs.tool_timeout_sec, 60);
    assert_eq!(docs.approval, crate::definition::McpApproval::Prompt);
    assert!(!docs.required);
    assert!(docs.args.is_empty());
    assert!(docs.env.is_empty());
    assert!(docs.tools.is_empty());

    let preset = config.preset(None).unwrap().unwrap();
    assert_eq!(preset.mcp_servers.len(), 1);
    assert_eq!(preset.mcp_servers[0].id, "git");
    let frozen = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(frozen.mcp_servers, preset.mcp_servers);

    let docs_only = config
        .definition(
            None,
            DefinitionOverrides {
                mcp: Some(vec!["docs".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(docs_only.mcp_servers.len(), 1);
    assert_eq!(docs_only.mcp_servers[0], *docs);
    let disabled = config
        .definition(
            None,
            DefinitionOverrides {
                mcp: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(disabled.mcp_servers.is_empty());
    assert!(
        serde_json::to_value(&disabled)
            .unwrap()
            .get("mcp_servers")
            .is_none()
    );
}

#[test]
fn wm11_tool_override_without_approval_inherits_the_server_setting() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\n").unwrap();
    std::fs::write(
        &json_path,
        r#"{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "approval": "writes",
      "enabledTools": ["git_status", "git_diff"],
      "tools": {
        "git_status": {},
        "git_diff": { "approval": "approve" }
      }
    },
    "docs": {
      "url": "https://example.invalid/mcp",
      "tools": { "search": {} }
    }
  }
}"#,
    )
    .unwrap();
    let config = Config::load(ConfigSource::File(toml_path)).unwrap();
    let git = &config.mcp_servers()["git"];
    assert_eq!(
        git.tools["git_status"].approval,
        crate::definition::McpApproval::Writes
    );
    assert_eq!(
        git.tools["git_diff"].approval,
        crate::definition::McpApproval::Approve
    );
    // Without a server-level setting the inherited value is the documented default Prompt.
    assert_eq!(
        config.mcp_servers()["docs"].tools["search"].approval,
        crate::definition::McpApproval::Prompt
    );
}

#[test]
fn wm11_claude_code_style_snippets_load_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\n").unwrap();
    std::fs::write(
        &json_path,
        r#"{
  "mcpServers": {
    "implicit-stdio": {
      "command": "node",
      "args": ["server.js", "--dir", "${WORK_DIR:-.}"]
    },
    "explicit-stdio": {
      "type": "stdio",
      "command": "python",
      "args": ["-m", "mcp_server"],
      "env": { "HOST": "${HOST:-127.0.0.1}" }
    },
    "implicit-http": {
      "url": "https://api.example.com/mcp",
      "headers": { "Authorization": "Bearer ${API_TOKEN}" }
    },
    "explicit-http": {
      "type": "http",
      "url": "https://api.example.com/mcp",
      "headers": { "X-API-Key": "${API_KEY}" }
    },
    "streamable-http": {
      "type": "streamable-http",
      "url": "https://api.example.com/streamable",
      "headers": { "Authorization": "Bearer ${STREAM_TOKEN}" }
    }
  }
}"#,
    )
    .unwrap();
    let config = Config::load(ConfigSource::File(toml_path)).unwrap();
    assert_eq!(config.mcp_servers().len(), 5);
    assert_eq!(
        config.mcp_servers()["implicit-stdio"].args,
        ["server.js", "--dir", "${WORK_DIR:-.}"]
    );
    assert_eq!(
        config.mcp_servers()["explicit-stdio"].env["HOST"],
        "${HOST:-127.0.0.1}"
    );
    assert_eq!(
        config.mcp_servers()["explicit-http"].headers["X-API-Key"],
        "${API_KEY}"
    );
}

#[test]
fn wm11_ignored_foreign_keys_produce_warnings_and_do_not_relax_settings() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\n").unwrap();
    std::fs::write(
        &json_path,
        r#"{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "disabled": true,
      "autoApprove": ["git_status"],
      "alwaysAllow": ["git_diff"],
      "trust": "all",
      "description": "Git server"
    }
  }
}"#,
    )
    .unwrap();
    let config = Config::load(ConfigSource::File(toml_path)).unwrap();
    let warnings = config.warnings();
    assert_eq!(warnings.len(), 5);
    for key in [
        "disabled",
        "autoApprove",
        "alwaysAllow",
        "trust",
        "description",
    ] {
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("git") && w.contains(key)),
            "warning for {key} missing: {warnings:?}"
        );
    }
    let git = &config.mcp_servers()["git"];
    assert_eq!(git.approval, crate::definition::McpApproval::Prompt);
    assert!(git.tools.is_empty());
}

#[test]
fn wm11_invalid_mcp_json_cases_are_rejected_without_echoing_values() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\n").unwrap();

    let secret = "secret_do_not_leak_9876";
    let cases = [
        // Unknown keys
        (r#"{"mcpServers": {"git": {"command": "uvx", "cwd": "/tmp"}}}"#, "cwd"),
        (r#"{"mcpServers": {"git": {"command": "uvx", "timeout": 10}}}"#, "startupTimeoutSec"),
        (r#"{"mcpServers": {"git": {"command": "uvx", "unknown_key": "x"}}}"#, "unknown key"),
        (r#"{"mcpServers": {}, "extra_root": true}"#, "extra_root"),
        // Invalid id
        (r#"{"mcpServers": {"Git": {"command": "uvx"}}}"#, "id"),
        (r#"{"mcpServers": {"git_server": {"command": "uvx"}}}"#, "id"),
        // Mutually exclusive command / url
        (r#"{"mcpServers": {"git": {"command": "uvx", "url": "https://example.invalid"}}}"#, "mutually exclusive"),
        (r#"{"mcpServers": {"git": {}}}"#, "exactly one"),
        // type errors
        (r#"{"mcpServers": {"git": {"type": "sse", "url": "https://example.invalid"}}}"#, "sse"),
        (r#"{"mcpServers": {"git": {"type": "http", "command": "uvx"}}}"#, "incompatible"),
        (r#"{"mcpServers": {"git": {"type": "stdio", "url": "https://example.invalid"}}}"#, "incompatible"),
        // Type specific field violations
        (r#"{"mcpServers": {"git": {"command": "uvx", "headers": {"X": "Y"}}}}"#, "not allowed for stdio"),
        (r#"{"mcpServers": {"git": {"url": "https://example.invalid", "args": ["a"]}}}"#, "not allowed for HTTP"),
        (r#"{"mcpServers": {"git": {"url": "https://example.invalid", "env": {"A": "B"}}}}"#, "not allowed for HTTP"),
        // enabledTools and disabledTools together
        (r#"{"mcpServers": {"git": {"command": "uvx", "enabledTools": ["a"], "disabledTools": ["b"]}}}"#, "mutually exclusive"),
        // Transport headers
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"Accept": "text/html"}}}}"#, "transport"),
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"Content-Type": "text/html"}}}}"#, "transport"),
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"Last-Event-Id": "1"}}}}"#, "transport"),
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"Mcp-Session-Id": "abc"}}}}"#, "transport"),
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"mcp-protocol-version": "1.0"}}}}"#, "transport"),
        // Duplicate headers case difference
        (r#"{"mcpServers": {"docs": {"url": "https://example.invalid", "headers": {"X-Header": "a", "x-header": "b"}}}}"#, "duplicate header"),
        // Literal secrets in Authorization
        (format!(r#"{{"mcpServers": {{"docs": {{"url": "https://example.invalid", "headers": {{"Authorization": "{secret}"}}}}}}}}"#).leak(), "Authorization"),
        (format!(r#"{{"mcpServers": {{"docs": {{"url": "https://example.invalid", "headers": {{"Authorization": "Bearer ${{TOKEN:-{secret}}}"}}}}}}}}"#).leak(), "Authorization"),
        // Literal secrets in X-API-Key
        (format!(r#"{{"mcpServers": {{"docs": {{"url": "https://example.invalid", "headers": {{"X-API-Key": "{secret}"}}}}}}}}"#).leak(), "X-API-Key"),
        (format!(r#"{{"mcpServers": {{"docs": {{"url": "https://example.invalid", "headers": {{"X-API-Key": "${{API_KEY:-{secret}}}"}}}}}}}}"#).leak(), "X-API-Key"),
        // Literal secrets in GITHUB_TOKEN env
        (format!(r#"{{"mcpServers": {{"git": {{"command": "uvx", "env": {{"GITHUB_TOKEN": "{secret}"}}}}}}}}"#).leak(), "GITHUB_TOKEN"),
        (format!(r#"{{"mcpServers": {{"git": {{"command": "uvx", "env": {{"GITHUB_TOKEN": "${{TOKEN:-{secret}}}"}}}}}}}}"#).leak(), "GITHUB_TOKEN"),
        // approval: "auto"
        (r#"{"mcpServers": {"git": {"command": "uvx", "approval": "auto"}}}"#, "approve"),
        (r#"{"mcpServers": {"git": {"command": "uvx", "tools": {"st": {"approval": "auto"}}}}}"#, "approve"),
        // command does not expand variables
        (r#"{"mcpServers": {"git": {"command": "uvx ${CMD}"}}}"#, "command does not expand"),
        // syntax error in template
        (r#"{"mcpServers": {"git": {"command": "uvx", "args": ["${UNCLOSED"]}}}"#, "variable syntax"),
    ];

    for (json_content, expected_needle) in cases {
        std::fs::write(&json_path, json_content).unwrap();
        let error = Config::load(ConfigSource::File(toml_path.clone()))
            .expect_err("must fail")
            .to_string();
        assert!(
            error
                .to_ascii_lowercase()
                .contains(&expected_needle.to_ascii_lowercase()),
            "expected needle {expected_needle:?} in error: {error}"
        );
        assert!(!error.contains(secret), "secret leaked in error: {error}");
    }
}

#[test]
fn wm11_config_toml_with_mcp_servers_table_fails_with_explicit_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    std::fs::write(
        &toml_path,
        "version = 1\n[mcp_servers.git]\ncommand = \"uvx\"\n",
    )
    .unwrap();
    let error = Config::load(ConfigSource::File(toml_path)).unwrap_err();
    assert!(matches!(error, super::ConfigError::McpConfiguredInToml));
    assert_eq!(error.to_string(), "MCP servers are configured in mcp.json");
}

#[test]
fn wm11_unknown_duplicate_and_invalid_mcp_selections_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&json_path, MCP_JSON).unwrap();

    std::fs::write(
        &toml_path,
        MCP_CONFIG.replace("mcp = [\"git\"]", "mcp = [\"absent\"]"),
    )
    .unwrap();
    let error = Config::load(ConfigSource::File(toml_path.clone())).expect_err("must reject");
    assert!(matches!(
        error,
        super::ConfigError::UnknownMcpServer(ref id) if id == "absent"
    ));

    std::fs::write(
        &toml_path,
        MCP_CONFIG.replace("mcp = [\"git\"]", "mcp = [\"git\", \"git\"]"),
    )
    .unwrap();
    let error = Config::load(ConfigSource::File(toml_path.clone())).expect_err("must reject");
    assert!(matches!(
        error,
        super::ConfigError::InvalidMcpServer(ref id) if id == "git"
    ));
}

#[test]
fn wm11_parse_mcp_boundaries_are_explicit() {
    assert!(super::parse_mcp("").unwrap().is_empty());
    assert!(super::parse_mcp("   ").unwrap().is_empty());
    assert_eq!(super::parse_mcp(" git , docs ").unwrap(), ["git", "docs"]);
    for value in [
        "git,,docs",
        ",git",
        "git,",
        "git,git",
        "Git",
        "git_hub",
        "git hub",
    ] {
        assert!(
            matches!(
                super::parse_mcp(value),
                Err(super::ConfigError::InvalidMcpServer(_))
            ),
            "{value:?}"
        );
    }
    assert!(matches!(
        super::parse_mcp(&"a".repeat(17)),
        Err(super::ConfigError::InvalidMcpServer(_))
    ));
}

#[test]
fn wm11_more_than_eight_selected_servers_fail_definition_validation() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");

    let mut servers_json = String::from("{\n  \"mcpServers\": {\n");
    for index in 0..9 {
        servers_json.push_str(&format!(
            "    \"server-{index}\": {{ \"url\": \"https://example.invalid/mcp\" }}{}\n",
            if index == 8 { "" } else { "," }
        ));
    }
    servers_json.push_str("  }\n}\n");
    std::fs::write(&json_path, servers_json).unwrap();

    let ids: Vec<String> = (0..9).map(|index| format!("\"server-{index}\"")).collect();
    let toml_text = format!(
        "version = 1\n[profiles.local]\nprovider = \"openai-chat\"\nmodel = \"m\"\nmcp = [{}]\n",
        ids.join(", ")
    );
    std::fs::write(&toml_path, toml_text).unwrap();

    assert!(matches!(
        Config::load(ConfigSource::File(toml_path)),
        Err(super::ConfigError::Definition(
            DefinitionError::InvalidMcpConfig
        ))
    ));
}

#[test]
fn wm11_selection_without_any_configured_servers_is_unknown() {
    let config = Config::load(ConfigSource::Disabled).unwrap();
    assert!(config.mcp_servers().is_empty());
    assert!(matches!(
        config.definition(
            None,
            DefinitionOverrides {
                provider: Some(Provider::OpenAiChat),
                model: Some("fixture".into()),
                mcp: Some(vec!["git".into()]),
                ..Default::default()
            },
        ),
        Err(super::ConfigError::UnknownMcpServer(id)) if id == "git"
    ));
}

#[test]
fn wm11_mcp_config_location_rules() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\n").unwrap();
    std::fs::write(&json_path, r#"{"mcpServers": {"git": {"command": "uvx"}}}"#).unwrap();

    // Sibling mcp.json in the same directory as an explicit config file is loaded.
    let config = Config::load(ConfigSource::File(toml_path.clone())).unwrap();
    assert_eq!(config.mcp_servers().len(), 1);
    assert_eq!(config.mcp_source(), Some(json_path.as_path()));

    // Missing mcp.json in the same directory yields empty servers without error.
    let dir2 = tempfile::tempdir().unwrap();
    let toml2 = dir2.path().join("config.toml");
    std::fs::write(&toml2, "version = 1\n").unwrap();
    let config2 = Config::load(ConfigSource::File(toml2)).unwrap();
    assert!(config2.mcp_servers().is_empty());
    assert_eq!(config2.mcp_source(), None);

    // Disabled config source never reads mcp.json even if present in the directory.
    let disabled = Config::load(ConfigSource::Disabled).unwrap();
    assert!(disabled.mcp_servers().is_empty());
    assert_eq!(disabled.mcp_source(), None);
}

#[test]
fn invalid_tool_names_are_derived_from_the_single_builtin_list() {
    let error = super::parse_tools("list_directory,bogus")
        .unwrap_err()
        .to_string();
    for tool in crate::definition::WorkspaceTool::ALL {
        assert!(
            error.contains(tool.name()),
            "{tool:?} missing from: {error}"
        );
    }
}

#[test]
fn parse_tools_and_toml_profile_resolve_find_files() {
    use crate::definition::WorkspaceTool;
    assert_eq!(
        super::parse_tools("find_files").unwrap(),
        vec![WorkspaceTool::FindFiles]
    );
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let file = directory.path().join("config.toml");
    let text = format!("{CONFIG}\nworkspace_root = \"workspace\"\ntools = [\"find_files\"]\n");
    std::fs::write(&file, text).unwrap();
    let config = Config::load(ConfigSource::File(file)).unwrap();
    let definition = config.definition(None, Default::default()).unwrap();
    assert_eq!(definition.tools, vec![WorkspaceTool::FindFiles]);
}

#[test]
fn command_profile_resolves_defaults_and_network_override() {
    use crate::definition::{CommandNetwork, CommandSettings, WorkspaceTool};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let file = directory.path().join("config.toml");
    let text = format!(
        "{CONFIG}\nworkspace_root = \"workspace\"\ntools = [\"run_command\"]\n[profiles.local.command]\nnetwork = \"none\"\ndefault_timeout_secs = 30\nmax_timeout_secs = 60\n"
    );
    std::fs::write(&file, text).unwrap();
    let config = Config::load(ConfigSource::File(file)).unwrap();
    let definition = config.definition(None, Default::default()).unwrap();
    assert_eq!(
        definition.command,
        Some(CommandSettings {
            network: CommandNetwork::None,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
        })
    );
    assert_eq!(definition.tools, vec![WorkspaceTool::RunCommand]);
    let overridden = config
        .definition(
            None,
            DefinitionOverrides {
                command_network: Some(CommandNetwork::Open),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(overridden.command.unwrap().network, CommandNetwork::Open);
    assert_eq!(overridden.command.unwrap().default_timeout_secs, 30);
}

#[test]
fn command_settings_without_run_command_are_refused_by_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let file = directory.path().join("config.toml");
    let text = format!(
        "{CONFIG}\nworkspace_root = \"workspace\"\ntools = [\"read_file\"]\n[profiles.local.command]\nmax_timeout_secs = 60\n"
    );
    std::fs::write(&file, text).unwrap();
    assert!(Config::load(ConfigSource::File(file)).is_err());
}

#[test]
fn compaction_profile_fields_resolve_and_invalid_values_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text =
        format!("{CONFIG}context_window = 200000\nauto_compact = false\ncompact_threshold = 55\n");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    let definition = config
        .definition(None, DefinitionOverrides::default())
        .unwrap();
    assert_eq!(definition.context_window, Some(200_000));
    assert!(!definition.auto_compact);
    assert_eq!(definition.compact_threshold, 55);
    let explicit = config
        .definition(
            None,
            DefinitionOverrides {
                context_window: Some(128_000),
                auto_compact: Some(true),
                compact_threshold: Some(80),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(explicit.context_window, Some(128_000));
    assert!(explicit.auto_compact);
    assert_eq!(explicit.compact_threshold, 80);
    let defaults = Config::load(ConfigSource::Disabled)
        .unwrap()
        .definition(
            None,
            DefinitionOverrides {
                provider: Some(Provider::OpenAiChat),
                model: Some("m".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(defaults.context_window, None);
    assert!(defaults.auto_compact);
    assert_eq!(defaults.compact_threshold, 70);
    for broken in [
        "context_window = 0",
        "compact_threshold = 9",
        "compact_threshold = 96",
    ] {
        std::fs::write(&path, format!("{CONFIG}{broken}\n")).unwrap();
        assert!(
            Config::load(ConfigSource::File(path.clone())).is_err()
                || config
                    .definition(None, DefinitionOverrides::default())
                    .is_err(),
            "accepted {broken}"
        );
    }
}

#[test]
fn external_tools_section_selects_the_ripgrep_source_and_defaults_to_managed() {
    use crate::external_tools::ToolSource;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    std::fs::write(&path, "version = 1\n").unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(config.ripgrep_source(), &ToolSource::Managed);
    assert_eq!(
        Config::load(ConfigSource::Disabled)
            .unwrap()
            .ripgrep_source(),
        &ToolSource::Managed
    );

    std::fs::write(&path, "version = 1\n[external_tools]\nrg = \"system\"\n").unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(config.ripgrep_source(), &ToolSource::System);

    // Platform-absolute path: `/opt/rg` is not absolute on Windows. The path is
    // built from the real temporary directory and written with forward slashes
    // so the TOML basic string needs no escaping.
    let absolute = std::env::temp_dir().join("tars-config-rg");
    let absolute_text = absolute.to_string_lossy().replace('\\', "/");
    std::fs::write(
        &path,
        format!("version = 1\n[external_tools]\nrg = \"{absolute_text}\"\n"),
    )
    .unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(
        config.ripgrep_source(),
        &ToolSource::Path(std::path::PathBuf::from(&absolute_text))
    );

    std::fs::write(&path, "version = 1\n[external_tools]\n").unwrap();
    let config = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(config.ripgrep_source(), &ToolSource::Managed);

    for text in [
        "version = 1\n[external_tools]\nrg = \"relative/rg\"\n",
        "version = 1\n[external_tools]\nrg = \"\"\n",
        "version = 1\n[external_tools]\nrg = \"~/bin/rg\"\n",
    ] {
        std::fs::write(&path, text).unwrap();
        let message = Config::load(ConfigSource::File(path.clone()))
            .expect_err("must reject")
            .to_string();
        assert!(message.contains("[external_tools] rg"), "{message}");
    }

    for text in [
        "version = 1\n[external_tools]\nrg = 5\n",
        "version = 1\n[external_tools]\nunknown = \"managed\"\n",
        "version = 1\n[external_tools]\nrg = \"managed\"\n[other]\nrg = \"managed\"\n",
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(Config::load(ConfigSource::File(path.clone())).is_err());
    }
}

#[test]
fn prelude_and_skills_top_level_and_profile_overrides_and_relative_paths() {
    use crate::prelude::{DynamicCustomSlot, StaticCustomSlot};

    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    let top_skills = cfg_dir.join("top-skills");
    let prof_skills = cfg_dir.join("prof-skills");
    let default_skills = cfg_dir.join("skills");
    std::fs::create_dir_all(&top_skills).unwrap();
    std::fs::create_dir_all(&prof_skills).unwrap();
    std::fs::create_dir_all(&default_skills).unwrap();
    std::fs::write(cfg_dir.join("sys.md"), "Top system from file").unwrap();
    std::fs::write(cfg_dir.join("dyn.md"), "Top dynamic from file").unwrap();
    std::fs::write(cfg_dir.join("prof-dyn.md"), "Profile dynamic from file").unwrap();

    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"
version = 1
default_profile = "base"

[prelude]
system_prompt_file = "sys.md"
dynamic_context_file = "dyn.md"

[skills]
dirs = ["top-skills"]
include_workspace = true

[profiles.base]
provider = "openai-chat"
model = "base-model"

[profiles.custom]
provider = "openai-chat"
model = "custom-model"
system_prompt = "Profile inline system"
dynamic_context_file = "prof-dyn.md"
skill_dirs = ["prof-skills"]
include_workspace_skills = false
"#,
    )
    .unwrap();

    let config = Config::load(ConfigSource::File(cfg_path)).unwrap();
    let canon_cfg_dir = cfg_dir.canonicalize().unwrap();
    assert_eq!(config.config_dir(), Some(canon_cfg_dir.as_path()));

    // Base profile inherits top-level [prelude] and [skills]
    let base_def = config
        .definition(Some("base"), DefinitionOverrides::default())
        .unwrap();
    assert_eq!(
        base_def.prelude.static_custom,
        Some(StaticCustomSlot::new("Top system from file"))
    );
    assert_eq!(
        base_def.prelude.dynamic_custom,
        Some(DynamicCustomSlot::File(
            cfg_dir.join("dyn.md").canonicalize().unwrap()
        ))
    );
    assert_eq!(base_def.prelude.config_dir, Some(canon_cfg_dir.clone()));
    assert_eq!(
        base_def.prelude.skill_dirs,
        vec![top_skills.canonicalize().unwrap()]
    );
    assert!(base_def.prelude.include_workspace_skills);
    assert!(base_def.prelude.skills_enabled);

    // Custom profile overrides top-level fields (even switching file <-> inline)
    let custom_def = config
        .definition(Some("custom"), DefinitionOverrides::default())
        .unwrap();
    assert_eq!(
        custom_def.prelude.static_custom,
        Some(StaticCustomSlot::new("Profile inline system"))
    );
    assert_eq!(
        custom_def.prelude.dynamic_custom,
        Some(DynamicCustomSlot::File(
            cfg_dir.join("prof-dyn.md").canonicalize().unwrap()
        ))
    );
    assert_eq!(
        custom_def.prelude.skill_dirs,
        vec![prof_skills.canonicalize().unwrap()]
    );
    assert!(!custom_def.prelude.include_workspace_skills);

    // When [skills] is omitted, default <config_dir>/skills is included if it exists
    let minimal_path = cfg_dir.join("minimal.toml");
    std::fs::write(
        &minimal_path,
        r#"
version = 1
[profiles.base]
provider = "openai-chat"
model = "base-model"
"#,
    )
    .unwrap();
    let minimal_cfg = Config::load(ConfigSource::File(minimal_path)).unwrap();
    let minimal_def = minimal_cfg
        .definition(Some("base"), DefinitionOverrides::default())
        .unwrap();
    assert_eq!(
        minimal_def.prelude.skill_dirs,
        vec![default_skills.canonicalize().unwrap()]
    );

    // Missing default <config_dir>/skills is silently skipped; explicit dirs=[] suppresses default
    std::fs::remove_dir_all(&default_skills).unwrap();
    let no_default_cfg = Config::load(ConfigSource::File(cfg_dir.join("minimal.toml"))).unwrap();
    let no_default_def = no_default_cfg
        .definition(Some("base"), DefinitionOverrides::default())
        .unwrap();
    assert!(no_default_def.prelude.skill_dirs.is_empty());
}

#[test]
fn prelude_and_skills_reject_same_level_conflicts_and_missing_or_wrong_type_explicit_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let existing_file = dir.path().join("file.txt");
    let existing_dir = dir.path().join("subdir");
    std::fs::write(&existing_file, "ok").unwrap();
    std::fs::create_dir_all(&existing_dir).unwrap();

    // Same-level inline + file conflict at top-level or profile
    for bad in [
        "version = 1\n[prelude]\nsystem_prompt = \"a\"\nsystem_prompt_file = \"file.txt\"\n",
        "version = 1\n[prelude]\ndynamic_context = \"a\"\ndynamic_context_file = \"file.txt\"\n",
        "version = 1\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"m\"\nsystem_prompt = \"a\"\nsystem_prompt_file = \"file.txt\"\n",
        "version = 1\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"m\"\ndynamic_context = \"a\"\ndynamic_context_file = \"file.txt\"\n",
        // Missing explicit paths and wrong types
        "version = 1\n[prelude]\nsystem_prompt_file = \"missing.md\"\n",
        "version = 1\n[prelude]\nsystem_prompt_file = \"subdir\"\n",
        "version = 1\n[prelude]\ndynamic_context_file = \"missing.md\"\n",
        "version = 1\n[prelude]\ndynamic_context_file = \"subdir\"\n",
        "version = 1\n[skills]\ndirs = [\"missing-dir\"]\n",
        "version = 1\n[skills]\ndirs = [\"file.txt\"]\n",
        // No ~ or $VAR expansion (literal missing path fails)
        "version = 1\n[skills]\ndirs = [\"~/skills\"]\n",
        "version = 1\n[prelude]\nsystem_prompt_file = \"$HOME/sys.md\"\n",
    ] {
        std::fs::write(&cfg_path, bad).unwrap();
        assert!(
            Config::load(ConfigSource::File(cfg_path.clone())).is_err(),
            "should reject:\n{bad}"
        );
    }
}

#[test]
fn user_tars_dir_prefers_absolute_tars_home_and_falls_back_to_dot_tars() {
    use super::file::resolve_user_tars_dir;
    use std::path::{Path, PathBuf};

    let tars_home = std::env::temp_dir().join("tars-home-override");
    let home = std::env::temp_dir().join("tars-user-home");

    assert_eq!(
        resolve_user_tars_dir(Some(&tars_home), Some(&home)),
        Some(tars_home.clone())
    );
    assert_eq!(
        resolve_user_tars_dir(Some(Path::new("relative-tars-home")), Some(&home)),
        Some(home.join(".tars"))
    );
    assert_eq!(
        resolve_user_tars_dir(Some(Path::new("")), Some(&home)),
        Some(home.join(".tars"))
    );
    assert_eq!(
        resolve_user_tars_dir(None, Some(&home)),
        Some(home.join(".tars"))
    );
    assert_eq!(resolve_user_tars_dir(None, Some(Path::new(""))), None);
    assert_eq!(resolve_user_tars_dir(None, Some(Path::new("."))), None);
    assert_eq!(
        resolve_user_tars_dir(None, Some(Path::new("relative-home"))),
        None
    );
    assert_eq!(
        resolve_user_tars_dir(Some(&PathBuf::from("relative")), None),
        None
    );
    assert_eq!(resolve_user_tars_dir(None, None), None);
}

#[test]
fn discover_uses_tars_home_then_home_dot_tars_and_handles_missing_roots() {
    const MODE: &str = "TARS_CONFIG_DISCOVER_TEST_MODE";
    const EXPECTED: &str = "TARS_CONFIG_DISCOVER_TEST_EXPECTED";
    if let Some(mode) = std::env::var_os(MODE) {
        let config = Config::load(ConfigSource::Discover).unwrap();
        match mode.to_str().unwrap() {
            "tars_home" | "home_fallback" => {
                let expected = std::path::PathBuf::from(std::env::var_os(EXPECTED).unwrap());
                let expected_canon = expected.canonicalize().unwrap();
                assert_eq!(
                    config.source(),
                    Some(expected.join("config.toml").as_path())
                );
                assert_eq!(config.config_dir(), Some(expected_canon.as_path()));
                assert_eq!(
                    config
                        .definition(None, DefinitionOverrides::default())
                        .unwrap()
                        .model,
                    "discovered-model"
                );
            }
            "missing" => {
                assert!(config.source().is_none());
                assert!(config.config_dir().is_none());
                assert!(config.preset(None).unwrap().is_none());
            }
            other => panic!("unexpected mode: {other}"),
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("custom-tars-home");
    std::fs::create_dir(&tars_home).unwrap();
    let toml_body = "version = 1\ndefault_profile = 'p'\n[profiles.p]\nprovider = 'openai-chat'\nmodel = 'discovered-model'\n";
    std::fs::write(tars_home.join("config.toml"), toml_body).unwrap();

    let user_home = dir.path().join("user-home");
    let dot_tars = user_home.join(".tars");
    std::fs::create_dir_all(&dot_tars).unwrap();
    std::fs::write(dot_tars.join("config.toml"), toml_body).unwrap();

    let run = |mode: &str,
               tars_home_env: Option<&str>,
               home_env: Option<&std::path::Path>,
               expected: Option<&std::path::Path>| {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.arg("--exact")
            .arg("config::tests::discover_uses_tars_home_then_home_dot_tars_and_handles_missing_roots")
            .env(MODE, mode)
            .env_remove("TARS_CONFIG")
            .env_remove("TARS_HOME")
            .env_remove("HOME")
            .env_remove("USERPROFILE");
        if let Some(value) = tars_home_env {
            cmd.env("TARS_HOME", value);
        }
        if let Some(home) = home_env {
            cmd.env("HOME", home).env("USERPROFILE", home);
        }
        if let Some(path) = expected {
            cmd.env(EXPECTED, path);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "mode={mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run(
        "tars_home",
        Some(tars_home.to_str().unwrap()),
        Some(&user_home),
        Some(&tars_home),
    );
    run(
        "home_fallback",
        Some("relative-ignored"),
        Some(&user_home),
        Some(&dot_tars),
    );
    run("home_fallback", Some(""), Some(&user_home), Some(&dot_tars));
    run("home_fallback", None, Some(&user_home), Some(&dot_tars));
    run("missing", None, None, None);
    run("missing", Some("relative-ignored"), None, None);
    run("missing", None, Some(std::path::Path::new(".")), None);
    run(
        "missing",
        Some(""),
        Some(std::path::Path::new("relative-home")),
        None,
    );
}

#[test]
fn code_execution_toml_defaults_profiles_overrides_and_validation() {
    use crate::definition::CodeExecutionSettings;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    // 1. Default disabled when omitted.
    std::fs::write(
        &path,
        r#"
version = 1
default_profile = "plain"
[profiles.plain]
provider = "openai-chat"
model = "m"
"#,
    )
    .unwrap();
    let cfg = Config::load(ConfigSource::File(path.clone())).unwrap();
    assert_eq!(
        cfg.definition(None, Default::default())
            .unwrap()
            .code_execution,
        None
    );

    // 2. Top-level [code_execution] + profile [profiles.<name>.code_execution] + overrides.
    std::fs::write(
        &path,
        r#"
version = 1
default_profile = "base"

[code_execution]
enabled = false
timeout_secs = 45
max_subcalls = 128

[profiles.base]
provider = "openai-chat"
model = "m"

[profiles.enabled_custom]
provider = "openai-chat"
model = "m"
[profiles.enabled_custom.code_execution]
enabled = true
memory_limit_bytes = 16777216
max_concurrency = 4
max_output_bytes = 8192
"#,
    )
    .unwrap();
    let cfg = Config::load(ConfigSource::File(path.clone())).unwrap();

    // Base inherits top-level disabled=false -> normalized to None.
    let base_def = cfg.definition(Some("base"), Default::default()).unwrap();
    assert_eq!(base_def.code_execution, None);

    // Explicit enable override activates top-level limits for base profile.
    let base_enabled = cfg
        .definition(
            Some("base"),
            DefinitionOverrides {
                code_execution_enabled: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        base_enabled.code_execution,
        Some(CodeExecutionSettings {
            timeout_secs: 45,
            max_subcalls: 128,
            ..CodeExecutionSettings::default()
        })
    );

    // Profile with enabled=true merges top-level + profile limits.
    let custom_def = cfg
        .definition(Some("enabled_custom"), Default::default())
        .unwrap();
    assert_eq!(
        custom_def.code_execution,
        Some(CodeExecutionSettings {
            timeout_secs: 45,
            memory_limit_bytes: 16 * 1024 * 1024,
            max_subcalls: 128,
            max_concurrency: 4,
            max_output_bytes: 8192,
            ..CodeExecutionSettings::default()
        })
    );

    // Explicit disable override turns off an enabled profile -> normalized to None.
    let custom_disabled = cfg
        .definition(
            Some("enabled_custom"),
            DefinitionOverrides {
                code_execution_enabled: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(custom_disabled.code_execution, None);

    // 3. Unknown field in [code_execution] or [profiles.<name>.code_execution] rejected.
    for bad_toml in [
        "version = 1\n[code_execution]\nunknown_key = 1\n",
        "version = 1\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"m\"\n[profiles.p.code_execution]\nunknown_key = 1\n",
    ] {
        std::fs::write(&path, bad_toml).unwrap();
        assert!(matches!(
            Config::load(ConfigSource::File(path.clone())),
            Err(super::ConfigError::Syntax { .. })
        ));
    }

    // 4. Out-of-range limits rejected even when enabled = false.
    for invalid_limits_toml in [
        "version = 1\n[code_execution]\nenabled = false\ntimeout_secs = 121\n",
        "version = 1\n[code_execution]\nmax_subcalls = 0\n",
        "version = 1\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"m\"\n[profiles.p.code_execution]\nenabled = false\nmemory_limit_bytes = 1024\n",
        "version = 1\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"m\"\n[profiles.p.code_execution]\nmax_output_bytes = 100\n",
    ] {
        std::fs::write(&path, invalid_limits_toml).unwrap();
        assert!(matches!(
            Config::load(ConfigSource::File(path.clone())),
            Err(super::ConfigError::Definition(
                DefinitionError::InvalidCodeExecutionSettings
            ))
        ));
    }
}

#[tokio::test]
async fn reopening_saved_session_preserves_saved_code_execution_over_toml_changes() {
    use crate::application::{ClosePolicy, PreparedCreate, SessionHost};
    use crate::definition::CodeExecutionSettings;
    use causa::kernel::Context;

    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
version = 1
default_profile = "dev"
[profiles.dev]
provider = "openai-chat"
model = "fixture"
api_key_env = "PATH"
[profiles.dev.code_execution]
enabled = true
timeout_secs = 22
max_subcalls = 64
"#,
    )
    .unwrap();

    let cfg = Config::load(ConfigSource::File(config_path.clone())).unwrap();
    let def = cfg.definition(None, Default::default()).unwrap();
    let expected = Some(CodeExecutionSettings {
        timeout_secs: 22,
        max_subcalls: 64,
        ..CodeExecutionSettings::default()
    });
    assert_eq!(def.code_execution, expected);

    let host = SessionHost::new(Some(store_dir.clone()));
    let created = host
        .create(PreparedCreate::persistent(def, Context::default()))
        .await
        .unwrap();
    let id = created.target().conversation_id().clone();
    host.close(&created.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Mutate TOML on disk to disable code_execution; reopening must keep saved definition.
    std::fs::write(
        &config_path,
        r#"
version = 1
default_profile = "dev"
[profiles.dev]
provider = "openai-chat"
model = "fixture"
api_key_env = "PATH"
[profiles.dev.code_execution]
enabled = false
"#,
    )
    .unwrap();
    let _reloaded_cfg = Config::load(ConfigSource::File(config_path)).unwrap();
    let host2 = SessionHost::new(Some(store_dir));
    let reopened = host2.open(id).await.unwrap();
    assert_eq!(reopened.definition().code_execution, expected);
    host2
        .close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[test]
fn agents_table_temperature_and_model_profiles_resolve_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    let global_agents = cfg_dir.join("global-agents");
    let profile_agents = cfg_dir.join("profile-agents");
    std::fs::create_dir_all(&global_agents).unwrap();
    std::fs::create_dir_all(&profile_agents).unwrap();

    let toml_path = cfg_dir.join("config.toml");
    std::fs::write(
        &toml_path,
        r#"
version = 1
default_profile = "main"

[agents]
dirs = ["global-agents"]
include_workspace = false

[profiles.main]
provider = "openai-chat"
model = "gpt-4o"
api_key_env = "TARS_MAIN_KEY"
temperature = 0.35
reasoning_effort = "medium"

[profiles.fast]
provider = "anthropic"
model = "claude-haiku"
api_key_env = "TARS_ANTHROPIC_KEY"
max_tokens = 1024
temperature = 0.8
agent_dirs = ["profile-agents"]
include_workspace_agents = true
"#,
    )
    .unwrap();

    let config = Config::load(ConfigSource::File(toml_path)).unwrap();
    let main_def = config.definition(None, Default::default()).unwrap();
    assert_eq!(main_def.temperature.map(|t| t.as_f32()), Some(0.35));
    assert_eq!(
        main_def.prelude.agent_dirs,
        vec![global_agents.canonicalize().unwrap()]
    );
    assert!(!main_def.prelude.include_workspace_agents);
    assert!(main_def.prelude.agents_enabled);

    let fast_def = config
        .definition(
            Some("fast"),
            DefinitionOverrides {
                temperature: Some(0.25),
                prelude: super::PreludeOverrides {
                    agents_enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(fast_def.temperature.map(|t| t.as_f32()), Some(0.25));
    assert_eq!(
        fast_def.prelude.agent_dirs,
        vec![profile_agents.canonicalize().unwrap()]
    );
    assert!(fast_def.prelude.include_workspace_agents);
    assert!(!fast_def.prelude.agents_enabled);

    let profiles = config.model_profiles();
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles["main"].model, "gpt-4o");
    assert_eq!(profiles["main"].temperature.map(|t| t.as_f32()), Some(0.35));
    assert_eq!(
        profiles["main"].credential,
        CredentialRef::Environment("TARS_MAIN_KEY".into())
    );
    assert_eq!(profiles["fast"].provider, Provider::Anthropic);
    assert_eq!(profiles["fast"].max_tokens, Some(1024));

    // Reject Anthropic profile temperature > 1.0
    let bad_toml = cfg_dir.join("bad.toml");
    std::fs::write(
        &bad_toml,
        r#"
version = 1
default_profile = "bad"
[profiles.bad]
provider = "anthropic"
model = "claude-haiku"
temperature = 1.5
"#,
    )
    .unwrap();
    assert!(Config::load(ConfigSource::File(bad_toml)).is_err());
}
