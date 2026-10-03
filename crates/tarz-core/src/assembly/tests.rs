use super::provider::{AssemblyError, assemble, assemble_with_runner};
use crate::definition::{CredentialRef, DefinitionError, Provider, SessionDefinition};
use crate::session::{SubmitRequest, WorkResult, WorkState};
use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelOutput, ModelRequest,
};
use causa::runtime::{ToolExecutor, TurnInterruption, TurnResult, TurnRunner};
use std::sync::Arc;
use std::time::Duration;

fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::Anthropic,
        model: "explicit-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment(format!(
            "TARZ_MISSING_{}",
            uuid::Uuid::now_v7().simple()
        )),
        work_timeout_secs: None,
        retained_work_capacity: 1,
        max_model_rounds: 0,
        max_tool_calls: 0,
    }
}

#[test]
fn invalid_definition_is_rejected_before_credential_resolution() {
    let mut definition = definition();
    definition.model.clear();
    assert!(matches!(
        assemble(&definition, "id".into(), Context::default()),
        Err(AssemblyError::Definition(DefinitionError::BlankModel))
    ));
}

#[test]
fn missing_credentials_report_only_a_sanitized_resolution_error() {
    let definition = definition();
    let error = match assemble(&definition, "id".into(), Context::default()) {
        Ok(_) => panic!("missing credential accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, AssemblyError::CredentialUnavailable));
    assert!(!error.to_string().contains("TARZ_MISSING_"));
}

struct MustNotInvoke;
#[async_trait]
impl ModelGateway for MustNotInvoke {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        panic!("zero saved model rounds must prevent invocation")
    }
}

#[tokio::test]
async fn assembled_work_uses_frozen_limits_and_supplied_conversation_identity() {
    let mut definition = definition();
    let runner = Arc::new(TurnRunner::new(
        Arc::new(MustNotInvoke),
        Arc::new(ToolExecutor::new(vec![], Default::default()).unwrap()),
    ));
    let mut session = assemble_with_runner(
        &definition,
        "supplied-id".into(),
        Context::default(),
        runner,
    );
    definition.max_model_rounds = 99;
    definition.retained_work_capacity = 0;
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "work".into(),
            blocks: vec![],
        })
        .unwrap();
    assert_eq!(receipt.work.conversation_id.as_str(), "supplied-id");
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("work unfinished")
    };
    assert!(matches!(
        &*result,
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxModelRounds { limit: 0 }
        })
    ));
    session.shutdown().await;
}
