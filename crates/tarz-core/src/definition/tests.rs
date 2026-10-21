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
