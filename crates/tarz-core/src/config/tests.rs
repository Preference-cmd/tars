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

    // Explicit override replaces preset
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

    // Exceeds max_write_bytes
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

    // Zero is rejected
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

    // Now rewrite config with a different permission mode
    let text2 = format!(
        "{CONFIG}permission_mode = \"read-only\"
"
    );
    std::fs::write(&cfg_path, &text2).unwrap();

    // Reopen session: it still uses its saved definition, unaffected by config change
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
