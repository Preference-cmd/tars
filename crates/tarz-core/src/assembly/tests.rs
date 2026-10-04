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
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
    }
}

#[test]
fn invalid_definition_is_rejected_before_credential_resolution() {
    let mut definition = definition();
    definition.model.clear();
    assert!(matches!(
        assemble(
            &definition,
            "id".into(),
            Context::default(),
            crate::tools::ToolResources::new(16)
        ),
        Err(AssemblyError::Definition(DefinitionError::BlankModel))
    ));
}

#[test]
fn missing_credentials_report_only_a_sanitized_resolution_error() {
    let definition = definition();
    let error = match assemble(
        &definition,
        "id".into(),
        Context::default(),
        crate::tools::ToolResources::new(16),
    ) {
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

#[test]
fn unavailable_workspace_is_rejected_before_credentials_or_model_access() {
    let mut definition = definition();
    definition.workspace_root = Some(
        std::env::current_dir()
            .unwrap()
            .join(uuid::Uuid::now_v7().to_string()),
    );
    definition.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    assert!(matches!(
        assemble(
            &definition,
            "id".into(),
            Context::default(),
            crate::tools::ToolResources::new(16)
        ),
        Err(AssemblyError::Workspace(_))
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn host_reopens_only_saved_workspace_definition_and_rejects_missing_root() {
    use crate::application::{ClosePolicy, PreparedCreate, SessionHost};
    let workspace = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = workspace.path().join("selected");
    std::fs::create_dir(&root).unwrap();
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(root.canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    chosen.tool_limits.max_concurrent_operations = 2;
    let saved_definition = chosen.clone();
    let host = SessionHost::new(Some(storage.path().to_owned()));
    let prepared = PreparedCreate::persistent(chosen, Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();
    assert_eq!(session.definition(), saved_definition);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host.open(id.clone()).await.unwrap();
    assert_eq!(reopened.definition(), saved_definition);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
    std::fs::remove_dir(&root).unwrap();
    let error = match host.open(id).await {
        Ok(_) => panic!("missing saved workspace accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("workspace resource unavailable"));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_rejects_enabled_workspace_but_still_assembles_empty_catalog() {
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(std::env::current_dir().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    assert!(matches!(
        assemble(&chosen, "id".into(), Context::default(), crate::tools::ToolResources::new(16)),
        Err(AssemblyError::Workspace(error)) if matches!(&*error, crate::workspace::WorkspaceError::UnsupportedPlatform)
    ));
    chosen.tools.clear();
    assert!(
        assemble(
            &chosen,
            "id".into(),
            Context::default(),
            crate::tools::ToolResources::new(16)
        )
        .is_ok()
    );
}
