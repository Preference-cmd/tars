use super::{CredentialRef, DefinitionError, Provider, SessionDefinition};

fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiResponses,
        model: "explicit-model".into(),
        endpoint: Some("https://example.test/v1/responses".into()),
        credential: CredentialRef::Environment("MODEL_API_KEY".into()),
        work_timeout_secs: Some(0),
        retained_work_capacity: 0,
        max_model_rounds: 0,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
    }
}

#[test]
fn roundtrip_preserves_explicit_execution_choices_and_only_credential_reference() {
    let original = definition();
    original.validate().unwrap();
    let encoded = serde_json::to_value(&original).unwrap();
    assert_eq!(encoded["provider"], "openai-responses");
    assert_eq!(encoded["credential"]["environment"], "MODEL_API_KEY");
    assert_eq!(
        serde_json::from_value::<SessionDefinition>(encoded).unwrap(),
        original
    );
    assert_eq!(original.session_config().retained_work_capacity, 0);
    assert_eq!(original.run_options().limits.max_model_rounds, 0);
    assert_eq!(original.run_options().limits.max_tool_calls, 0);
}

#[test]
fn unknown_or_missing_choices_do_not_silently_use_new_defaults() {
    let mut encoded = serde_json::to_value(definition()).unwrap();
    encoded["unknown"] = true.into();
    assert!(serde_json::from_value::<SessionDefinition>(encoded.clone()).is_err());
    encoded.as_object_mut().unwrap().remove("unknown");
    encoded.as_object_mut().unwrap().remove("max_tool_calls");
    assert!(serde_json::from_value::<SessionDefinition>(encoded).is_err());
}

#[test]
fn credential_bearing_and_non_http_endpoints_are_rejected_without_echoing_values() {
    for endpoint in [
        "https://name:secret@example.test/v1",
        "https://name@example.test/v1",
        "https://example.test/v1?api_key=secret",
        "https://example.test/v1#secret",
        "file:///tmp/key",
        "not a URL",
        " https://example.test/v1",
    ] {
        let mut candidate = definition();
        candidate.endpoint = Some(endpoint.into());
        let error = candidate.validate().unwrap_err();
        assert_eq!(error, DefinitionError::InvalidEndpoint);
        assert!(!error.to_string().contains("secret"));
        assert!(!error.to_string().contains(endpoint));
    }
}

#[test]
fn invalid_model_reference_and_unrepresentable_deadline_are_rejected() {
    let mut candidate = definition();
    candidate.model = "  ".into();
    assert_eq!(candidate.validate(), Err(DefinitionError::BlankModel));
    candidate.model = "model".into();
    for name in ["", "1KEY", "API-KEY", "A=B", "key with spaces", "密钥"] {
        candidate.credential = CredentialRef::Environment(name.into());
        assert_eq!(
            candidate.validate(),
            Err(DefinitionError::InvalidCredentialReference)
        );
    }
    candidate.credential = CredentialRef::Environment("_KEY2".into());
    candidate.work_timeout_secs = Some(u64::MAX);
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::UnrepresentableTimeout)
    );
}

#[test]
fn workspace_permissions_require_explicit_root_and_bounded_limits() {
    use super::{WorkspaceTool, WorkspaceToolLimits};
    let mut candidate = definition();
    candidate.tools = vec![WorkspaceTool::ReadFile];
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::WorkspaceRequired)
    );
    candidate.workspace_root = Some("relative".into());
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::InvalidWorkspaceRoot)
    );
    candidate.workspace_root = Some(std::env::current_dir().unwrap());
    assert!(candidate.validate().is_ok());
    candidate.tools.push(WorkspaceTool::ReadFile);
    assert_eq!(candidate.validate(), Err(DefinitionError::DuplicateTool));
    candidate.tools.pop();
    candidate.tool_limits.max_concurrent_operations = 5;
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::InvalidToolLimits)
    );
    candidate.tool_limits = WorkspaceToolLimits::default();
    candidate.tool_limits.call_timeout_secs = 0;
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::InvalidToolLimits)
    );
    let mut encoded = serde_json::to_value(definition()).unwrap();
    encoded.as_object_mut().unwrap().remove("tool_limits");
    assert!(serde_json::from_value::<SessionDefinition>(encoded).is_err());
}

#[test]
fn persisted_no_workspace_must_be_explicit_null() {
    let mut encoded = serde_json::to_value(definition()).unwrap();
    assert!(encoded["workspace_root"].is_null());
    encoded.as_object_mut().unwrap().remove("workspace_root");
    assert!(serde_json::from_value::<SessionDefinition>(encoded).is_err());
}

#[test]
fn generation_choices_reach_run_options_and_have_no_default() {
    let mut candidate = definition();
    candidate.provider = Provider::Anthropic;
    // No limit is invented, so the Anthropic renderer rejects the request.
    assert_eq!(candidate.run_options().generation.max_tokens, None);
    assert_eq!(candidate.run_options().generation.reasoning_effort, None);

    candidate.max_tokens = Some(2048);
    candidate.reasoning_effort = Some("high".into());
    candidate.validate().unwrap();
    let generation = candidate.run_options().generation;
    assert_eq!(generation.max_tokens, Some(2048));
    assert_eq!(generation.reasoning_effort, Some("high".into()));
    let encoded = serde_json::to_value(&candidate).unwrap();
    assert_eq!(encoded["max_tokens"], 2048);
    assert_eq!(encoded["reasoning_effort"], "high");
    assert_eq!(
        serde_json::from_value::<SessionDefinition>(encoded).unwrap(),
        candidate
    );

    candidate.max_tokens = Some(0);
    assert_eq!(
        candidate.validate().unwrap_err(),
        DefinitionError::InvalidMaxTokens
    );
}

#[test]
fn a_record_saved_before_the_generation_fields_reads_them_as_unset() {
    let mut encoded = serde_json::to_value(definition()).unwrap();
    let object = encoded.as_object_mut().unwrap();
    object.remove("max_tokens").unwrap();
    object.remove("reasoning_effort").unwrap();
    let read = serde_json::from_value::<SessionDefinition>(encoded).unwrap();
    assert_eq!(read, definition());
}

#[test]
fn the_attempt_limit_is_optional_positive_and_absent_from_older_records() {
    let mut candidate = definition();
    candidate.max_model_attempts = Some(0);
    assert_eq!(
        candidate.validate().unwrap_err(),
        DefinitionError::InvalidModelAttempts
    );
    candidate.max_model_attempts = Some(3);
    candidate.validate().unwrap();
    let mut encoded = serde_json::to_value(&candidate).unwrap();
    assert_eq!(encoded["max_model_attempts"], 3);
    encoded
        .as_object_mut()
        .unwrap()
        .remove("max_model_attempts")
        .unwrap();
    let read = serde_json::from_value::<SessionDefinition>(encoded).unwrap();
    assert_eq!(read.max_model_attempts, None);
}

#[test]
fn render_options_are_read_as_the_protocol_type_and_kept_as_written() {
    use super::RenderOptions;
    use causa::providers::ChatMaxTokensField;
    let mut candidate = definition();
    candidate.provider = Provider::OpenAiChat;
    assert!(matches!(
        candidate.protocol_render_options().unwrap(),
        RenderOptions::Chat(options) if options.max_tokens_field == ChatMaxTokensField::MaxCompletionTokens
    ));
    // An empty table is left out of the record.
    assert!(
        serde_json::to_value(&candidate)
            .unwrap()
            .get("render_options")
            .is_none()
    );

    candidate.render_options =
        serde_json::from_value(serde_json::json!({"max_tokens_field": "max_tokens"})).unwrap();
    candidate.validate().unwrap();
    let RenderOptions::Chat(options) = candidate.protocol_render_options().unwrap() else {
        panic!("chat options");
    };
    assert_eq!(options.max_tokens_field, ChatMaxTokensField::MaxTokens);
    // Only the written key is saved; defaults are not expanded into the record.
    let mut encoded = serde_json::to_value(&candidate).unwrap();
    assert_eq!(
        encoded["render_options"],
        serde_json::json!({"max_tokens_field": "max_tokens"})
    );
    assert_eq!(
        serde_json::from_value::<SessionDefinition>(encoded.clone()).unwrap(),
        candidate
    );
    // A record saved before the field existed reads as empty.
    encoded.as_object_mut().unwrap().remove("render_options");
    let read = serde_json::from_value::<SessionDefinition>(encoded).unwrap();
    assert!(read.render_options.is_empty());

    let mut responses = definition();
    responses.render_options = serde_json::from_value(serde_json::json!({"store": true})).unwrap();
    assert!(matches!(
        responses.protocol_render_options().unwrap(),
        RenderOptions::Responses(options) if options.store == Some(true)
    ));
}

#[test]
fn render_options_the_protocol_does_not_accept_are_rejected_without_echoing_them() {
    for (provider, table) in [
        (
            Provider::OpenAiChat,
            serde_json::json!({"secret_marker_key": true}),
        ),
        (
            Provider::OpenAiChat,
            serde_json::json!({"max_tokens_field": "secret_marker_value"}),
        ),
        (
            Provider::OpenAiChat,
            serde_json::json!({"max_tokens_field": 300}),
        ),
        // A Chat option is unknown to the other protocols.
        (
            Provider::Anthropic,
            serde_json::json!({"max_tokens_field": "max_tokens"}),
        ),
        (
            Provider::OpenAiResponses,
            serde_json::json!({"max_tokens_field": "max_tokens"}),
        ),
    ] {
        let mut candidate = definition();
        candidate.provider = provider;
        candidate.render_options = serde_json::from_value(table).unwrap();
        let error = candidate.validate().unwrap_err();
        assert_eq!(error, DefinitionError::InvalidRenderOptions);
        assert!(!error.to_string().contains("secret_marker"));
    }
}

#[test]
fn write_tools_are_accepted_during_definition_validation() {
    use super::WorkspaceTool;

    let mut def = definition();
    def.workspace_root = Some(std::path::PathBuf::from("/workspace"));
    def.tools = vec![WorkspaceTool::WriteFile];
    assert!(def.validate().is_ok());

    def.tools = vec![WorkspaceTool::EditFile];
    assert!(def.validate().is_ok());

    def.tools = vec![
        WorkspaceTool::ReadFile,
        WorkspaceTool::WriteFile,
        WorkspaceTool::EditFile,
    ];
    assert!(def.validate().is_ok());
}

#[test]
fn write_tool_limits_validation_and_serialization() {
    use super::WriteToolLimits;

    let default_limits = WriteToolLimits::default();
    assert_eq!(default_limits.max_edit_file_bytes, 2 * 1024 * 1024);
    assert_eq!(default_limits.max_write_bytes, 1024 * 1024);
    assert_eq!(default_limits.max_edit_segments, 32);
    assert!(default_limits.validate().is_ok());

    // Rejects zero
    let zero_edit = WriteToolLimits {
        max_edit_file_bytes: 0,
        ..default_limits
    };
    assert_eq!(
        zero_edit.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    let zero_write = WriteToolLimits {
        max_write_bytes: 0,
        ..default_limits
    };
    assert_eq!(
        zero_write.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    let zero_segments = WriteToolLimits {
        max_edit_segments: 0,
        ..default_limits
    };
    assert_eq!(
        zero_segments.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    // Rejects above ceiling
    let high_edit = WriteToolLimits {
        max_edit_file_bytes: default_limits.max_edit_file_bytes + 1,
        ..default_limits
    };
    assert_eq!(
        high_edit.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    let high_write = WriteToolLimits {
        max_write_bytes: default_limits.max_write_bytes + 1,
        ..default_limits
    };
    assert_eq!(
        high_write.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    let high_segments = WriteToolLimits {
        max_edit_segments: default_limits.max_edit_segments + 1,
        ..default_limits
    };
    assert_eq!(
        high_segments.validate().unwrap_err(),
        DefinitionError::InvalidToolLimits
    );

    // Serde roundtrip on SessionDefinition
    let mut def = definition();
    def.write_limits = Some(WriteToolLimits {
        max_edit_file_bytes: 1024 * 1024,
        max_write_bytes: 512 * 1024,
        max_edit_segments: 16,
    });
    assert!(def.validate().is_ok());
    let json_val = serde_json::to_value(&def).unwrap();
    assert_eq!(json_val["write_limits"]["max_edit_segments"], 16);
    let roundtripped: SessionDefinition = serde_json::from_value(json_val).unwrap();
    assert_eq!(roundtripped.write_limits, def.write_limits);
}

fn stdio_server() -> super::McpServerConfig {
    use std::collections::BTreeMap;
    super::McpServerConfig {
        id: "git".into(),
        command: Some("uvx".into()),
        args: vec!["mcp-server-git".into(), "--repository".into(), ".".into()],
        url: None,
        env: BTreeMap::from([
            ("LOG_LEVEL".into(), "warn".into()),
            ("GIT_AUTHOR_NAME".into(), "${GIT_AUTHOR_NAME}".into()),
        ]),
        headers: BTreeMap::new(),
        startup_timeout_sec: 10,
        tool_timeout_sec: 60,
        enabled_tools: None,
        disabled_tools: None,
        approval: super::McpApproval::Prompt,
        tools: BTreeMap::new(),
        required: false,
    }
}

fn http_server() -> super::McpServerConfig {
    use std::collections::BTreeMap;
    super::McpServerConfig {
        id: "docs".into(),
        command: None,
        args: vec![],
        url: Some("https://example.invalid/mcp".into()),
        env: Default::default(),
        headers: BTreeMap::from([("Authorization".into(), "Bearer ${DOCS_MCP_TOKEN}".into())]),
        startup_timeout_sec: 10,
        tool_timeout_sec: 60,
        enabled_tools: Some(vec!["search".into()]),
        disabled_tools: None,
        approval: super::McpApproval::Writes,
        tools: Default::default(),
        required: true,
    }
}

#[test]
fn wm11_valid_stdio_and_http_servers_pass_and_definition_roundtrips() {
    let mut candidate = definition();
    candidate.mcp_servers = vec![stdio_server(), http_server()];
    candidate.validate().unwrap();

    let encoded = serde_json::to_value(&candidate).unwrap();
    let stdio = &encoded["mcp_servers"][0];
    assert_eq!(stdio["id"], "git");
    assert_eq!(stdio["command"], "uvx");
    assert_eq!(stdio["startup_timeout_sec"], 10);
    assert_eq!(stdio["tool_timeout_sec"], 60);
    assert_eq!(stdio["approval"], "prompt");
    assert_eq!(stdio["required"], false);
    assert!(stdio.get("url").is_none());
    assert!(stdio.get("headers").is_none());
    assert!(stdio.get("enabled_tools").is_none());
    assert!(stdio.get("disabled_tools").is_none());

    let http = &encoded["mcp_servers"][1];
    assert_eq!(http["id"], "docs");
    assert_eq!(http["url"], "https://example.invalid/mcp");
    assert_eq!(http["headers"]["Authorization"], "Bearer ${DOCS_MCP_TOKEN}");
    assert_eq!(http["approval"], "writes");
    assert_eq!(http["required"], true);
    // Empty stdio-only collections are omitted from a remote snapshot.
    for key in ["command", "args", "env", "tools"] {
        assert!(http.get(key).is_none(), "{key} must be omitted");
    }

    let read = serde_json::from_value::<SessionDefinition>(encoded).unwrap();
    assert_eq!(read, candidate);
}

#[test]
fn wm10_servers_omitted_from_a_record_read_as_empty() {
    let encoded = serde_json::to_value(definition()).unwrap();
    assert!(encoded.get("mcp_servers").is_none());
    let read = serde_json::from_value::<SessionDefinition>(encoded).unwrap();
    assert!(read.mcp_servers.is_empty());
    assert!(
        serde_json::to_value(&read)
            .unwrap()
            .get("mcp_servers")
            .is_none()
    );
}

#[test]
fn wm10_stage_g_record_converts_env_vars_bearer_and_auto_approval() {
    let raw = serde_json::json!({
        "id": "git",
        "command": "uvx",
        "args": ["mcp-git"],
        "env": { "LOG_LEVEL": "info", "OVERWRITE": "old_literal" },
        "env_vars": ["GIT_USER", "OVERWRITE"],
        "startup_timeout_sec": 15,
        "tool_timeout_sec": 45,
        "approval": "auto",
        "tools": {
            "status": { "approval": "auto" }
        },
        "required": false
    });
    let server: super::McpServerConfig = serde_json::from_value(raw).unwrap();
    assert_eq!(server.id, "git");
    assert_eq!(server.approval, super::McpApproval::Approve);
    assert_eq!(server.tools["status"].approval, super::McpApproval::Approve);
    assert_eq!(server.env["LOG_LEVEL"], "info");
    assert_eq!(server.env["GIT_USER"], "${GIT_USER}");
    // env_vars overrides same-name env literal
    assert_eq!(server.env["OVERWRITE"], "${OVERWRITE}");

    let raw_http = serde_json::json!({
        "id": "docs",
        "url": "https://example.invalid/mcp",
        "bearer_token_env_var": "DOCS_TOKEN",
        "startup_timeout_sec": 10,
        "tool_timeout_sec": 60,
        "approval": "prompt",
        "required": true
    });
    let server_http: super::McpServerConfig = serde_json::from_value(raw_http).unwrap();
    assert_eq!(server_http.headers["Authorization"], "Bearer ${DOCS_TOKEN}");

    // Reserialization produces new shape and "approve", without old fields
    let reserialized = serde_json::to_value(&server).unwrap();
    assert_eq!(reserialized["approval"], "approve");
    assert_eq!(reserialized["tools"]["status"]["approval"], "approve");
    assert!(reserialized.get("env_vars").is_none());
    assert!(reserialized.get("bearer_token_env_var").is_none());

    // Unknown fields in records are still rejected
    let bad = serde_json::json!({
        "id": "git",
        "command": "uvx",
        "startup_timeout_sec": 10,
        "tool_timeout_sec": 60,
        "approval": "prompt",
        "required": false,
        "unknown_extra_field": true
    });
    assert!(serde_json::from_value::<super::McpServerConfig>(bad).is_err());
}

#[test]
fn wm11_server_id_length_and_character_rules_are_enforced() {
    for id in [
        "".to_owned(),
        "Git".to_owned(),
        "git_hub".to_owned(),
        "git hub".to_owned(),
        "git.server".to_owned(),
        "a".repeat(16),
        "a".repeat(17),
    ] {
        let mut candidate = stdio_server();
        candidate.id = id;
        let expected_valid = candidate.id.len() == 16;
        assert_eq!(
            candidate.validate().is_ok(),
            expected_valid,
            "id {:?}",
            candidate.id
        );
    }
}

#[test]
fn wm11_command_and_url_are_exclusive_and_type_specific_fields_are_rejected() {
    let mut candidate = stdio_server();
    candidate.url = Some("https://example.invalid/mcp".into());
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    let mut candidate = stdio_server();
    candidate.command = None;
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    for command in ["", "uvx\0hidden", "uvx ${CMD}"] {
        let mut candidate = stdio_server();
        candidate.command = Some(command.into());
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    }

    let mut candidate = stdio_server();
    candidate.headers = std::collections::BTreeMap::from([("X-Custom".into(), "val".into())]);
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    for mutate in [
        (|server: &mut super::McpServerConfig| server.args = vec!["x".into()])
            as fn(&mut super::McpServerConfig),
        |server: &mut super::McpServerConfig| {
            server.env = std::collections::BTreeMap::from([("LOG".into(), "x".into())]);
        },
    ] {
        let mut candidate = http_server();
        mutate(&mut candidate);
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    }
}

#[test]
fn wm11_remote_urls_reuse_the_endpoint_rules() {
    for url in [
        "https://name:secret@example.invalid/mcp",
        "https://name@example.invalid/mcp",
        "https://example.invalid/mcp?token=secret",
        "https://example.invalid/mcp#secret",
        "file:///tmp/mcp",
        "not a URL",
        " https://example.invalid/mcp",
    ] {
        let mut candidate = http_server();
        candidate.url = Some(url.into());
        let error = candidate.validate().unwrap_err();
        assert_eq!(error, DefinitionError::InvalidMcpConfig, "url {url:?}");
        assert!(!error.to_string().contains("secret"));
    }
    for url in [
        "http://127.0.0.1:1/mcp",
        "https://example.invalid/mcp",
        "https://example.invalid/${DOCS_PATH}",
        "https://${DOCS_HOST}/mcp",
        "${DOCS_URL}",
    ] {
        let mut candidate = http_server();
        candidate.url = Some(url.into());
        assert!(candidate.validate().is_ok(), "{url}");
    }
}

#[test]
fn wm11_timeout_environment_and_filter_fields_are_bounded() {
    for (startup, tool) in [(0, 60), (61, 60), (10, 0), (10, 3601)] {
        let mut candidate = stdio_server();
        candidate.startup_timeout_sec = startup;
        candidate.tool_timeout_sec = tool;
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    }
    for name in ["", "1KEY", "API-KEY", "A=B", "key with spaces"] {
        let mut candidate = stdio_server();
        candidate.env = std::collections::BTreeMap::from([(name.into(), "value".into())]);
        assert_eq!(
            candidate.validate(),
            Err(DefinitionError::InvalidMcpConfig),
            "env var name {name:?}"
        );
    }
    // Template syntax error in env or args
    for bad_template in ["${UNCLOSED", "${123}", "${FOO${BAR}}"] {
        let mut candidate = stdio_server();
        candidate.env =
            std::collections::BTreeMap::from([("VALID_NAME".into(), bad_template.into())]);
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

        let mut candidate = stdio_server();
        candidate.args = vec![bad_template.into()];
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    }
    // Headers validation
    for (name, val) in [
        ("Bad Name", "value"),
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("Last-Event-Id", "123"),
        ("Mcp-Protocol-Version", "1.0"),
        ("mcp-custom", "value"),
        ("Valid-Header", "bad\nvalue"),
        ("Valid-Header", "bad\rvalue"),
        ("Valid-Header", "${UNCLOSED"),
    ] {
        let mut candidate = http_server();
        candidate.headers = std::collections::BTreeMap::from([(name.into(), val.into())]);
        assert_eq!(
            candidate.validate(),
            Err(DefinitionError::InvalidMcpConfig),
            "header {name}: {val}"
        );
    }

    let mut candidate = stdio_server();
    candidate.enabled_tools = Some(vec!["status".into()]);
    candidate.disabled_tools = Some(vec!["diff".into()]);
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    for filter in [
        vec![],
        vec!["".into()],
        vec!["status".into(), "status".into()],
    ] {
        let mut candidate = stdio_server();
        candidate.enabled_tools = Some(filter.clone());
        assert_eq!(
            candidate.validate(),
            Err(DefinitionError::InvalidMcpConfig),
            "enabled {filter:?}"
        );
        let mut candidate = stdio_server();
        candidate.disabled_tools = Some(filter);
        assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    }
}

#[test]
fn wm11_tool_overrides_must_agree_with_the_selected_filter() {
    use super::{McpApproval, McpToolSetting};
    use std::collections::BTreeMap;

    let mut candidate = stdio_server();
    candidate.tools = BTreeMap::from([(
        "".into(),
        McpToolSetting {
            approval: McpApproval::Approve,
        },
    )]);
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    let mut candidate = stdio_server();
    candidate.enabled_tools = Some(vec!["status".into()]);
    candidate.tools = BTreeMap::from([(
        "diff".into(),
        McpToolSetting {
            approval: McpApproval::Approve,
        },
    )]);
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    let mut candidate = stdio_server();
    candidate.disabled_tools = Some(vec!["diff".into()]);
    candidate.tools = BTreeMap::from([(
        "diff".into(),
        McpToolSetting {
            approval: McpApproval::Approve,
        },
    )]);
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    candidate.disabled_tools = Some(vec!["other".into()]);
    assert!(candidate.validate().is_ok());
    let encoded = serde_json::to_value(&candidate).unwrap();
    assert_eq!(encoded["tools"]["diff"]["approval"], "approve");
}

#[test]
fn wm11_duplicate_ids_and_the_server_count_limit_are_rejected() {
    let mut candidate = definition();
    candidate.mcp_servers = vec![stdio_server(), stdio_server()];
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));

    let mut candidate = definition();
    candidate.mcp_servers = (0..9)
        .map(|index| {
            let mut server = http_server();
            server.id = format!("server-{index}");
            server
        })
        .collect();
    assert_eq!(candidate.validate(), Err(DefinitionError::InvalidMcpConfig));
    candidate.mcp_servers.truncate(8);
    assert!(candidate.validate().is_ok());
}

#[test]
fn wm11_mcp_errors_never_echo_environment_values() {
    let marker = "mcp-secret-value-marker";
    let mut candidate = definition();
    let mut server = stdio_server();
    server.env = std::collections::BTreeMap::from([(marker.into(), marker.into())]);
    server.tool_timeout_sec = 0;
    candidate.mcp_servers = vec![server];
    let error = candidate.validate().unwrap_err();
    assert_eq!(error, DefinitionError::InvalidMcpConfig);
    assert!(!error.to_string().contains(marker));
}

#[test]
fn wm11_redact_substitutions_rules() {
    use super::{expand_template, redact_substitutions};
    assert_eq!(expand_template("literal").unwrap(), "literal");

    let substitutions = vec![
        ("${SHORT}".to_string(), "abc".to_string()),
        ("${LONG}".to_string(), "secret_token_12345".to_string()),
        ("${SUB}".to_string(), "secret".to_string()),
    ];

    let text = "error connecting to http://127.0.0.1/path/secret_token_12345/sub/abc";
    let redacted = redact_substitutions(text, &substitutions);

    assert!(redacted.contains("${LONG}"));
    assert!(!redacted.contains("secret_token_12345"));
    assert!(redacted.contains("/abc"));
    assert!(!redacted.contains("${SHORT}"));
}

#[test]
fn command_settings_roundtrip_old_records_and_validation() {
    use super::{CommandNetwork, CommandSettings, WorkspaceTool};

    let mut candidate = definition();
    candidate.workspace_root = Some(std::env::current_dir().unwrap());
    candidate.tools = vec![WorkspaceTool::RunCommand];
    candidate.command = Some(CommandSettings {
        network: CommandNetwork::Open,
        default_timeout_secs: 30,
        max_timeout_secs: 60,
    });
    candidate.validate().unwrap();
    let encoded = serde_json::to_value(&candidate).unwrap();
    assert_eq!(encoded["command"]["network"], "open");
    assert_eq!(encoded["command"]["default_timeout_secs"], 30);
    assert_eq!(
        serde_json::from_value::<SessionDefinition>(encoded).unwrap(),
        candidate
    );

    let old = serde_json::to_value(definition()).unwrap();
    assert!(old.get("command").is_none());
    let old = serde_json::from_value::<SessionDefinition>(old).unwrap();
    assert_eq!(old.command, None);

    candidate.command = None;
    candidate.validate().unwrap();
    assert_eq!(
        candidate.effective_command_settings(),
        Some(CommandSettings::default())
    );

    candidate.command = Some(CommandSettings::default());
    candidate.tools.clear();
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::CommandSettingsWithoutTool)
    );

    candidate.tools = vec![WorkspaceTool::RunCommand];
    candidate.command = Some(CommandSettings {
        default_timeout_secs: 61,
        max_timeout_secs: 60,
        ..CommandSettings::default()
    });
    assert_eq!(
        candidate.validate(),
        Err(DefinitionError::InvalidCommandSettings)
    );
}
