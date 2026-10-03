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
