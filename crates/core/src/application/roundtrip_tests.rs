use super::{ClosePolicy, PreparedCreate, SaveStatus, SessionHost, text_input};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::session::{Session, SessionError, SubmitRequest, WorkResult, WorkState};
use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockMeta, CallControl, Context, ContextBlock, ModelGateway, ModelInvokeError,
    ModelItem, ModelOutput, ModelRequest, ModelResponse, ModelStopReason, Tool, ToolCallContext,
    ToolCallPayload, ToolDefinition, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::runtime::{ToolExecutor, TurnResult, TurnRunner, new_block_id};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Gateway {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
#[async_trait]
impl ModelGateway for Gateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("fixed definition completion")],
            },
            ModelStopReason::EndTurn,
        ))
    }
}
struct CountedOldTool(Arc<AtomicUsize>);
#[async_trait]
impl Tool for CountedOldTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "old-tool",
            "Previously completed fixture tool",
            serde_json::json!({"type":"object"}),
        )
    }
    async fn execute(
        &self,
        call: &ToolCallContext,
        _: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolResultPayload::new(
            call.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({"unexpected_reexecution":true})),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_reopen_preserves_definition_material_identity_and_restores_no_work_or_tool_execution()
 {
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let definition = SessionDefinition {
        provider: Provider::Anthropic,
        model: "roundtrip-frozen-model".into(),
        endpoint: Some("https://provider.invalid/custom/messages".into()),
        credential: CredentialRef::Environment("TARS_ROUNDTRIP_REFERENCE".into()),
        work_timeout_secs: Some(17),
        retained_work_capacity: 3,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
    };
    let requests = Arc::new(Mutex::new(vec![]));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let assemblies = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(TurnRunner::new(
        Arc::new(Gateway {
            requests: requests.clone(),
        }),
        Arc::new(
            ToolExecutor::new(
                vec![Arc::new(CountedOldTool(tool_calls.clone()))],
                Default::default(),
            )
            .unwrap(),
        ),
    ));
    let assembly_count = assemblies.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        assembly_count.fetch_add(1, Ordering::SeqCst);
        let runner = runner.clone();
        let options = definition.run_options();
        Ok(Session::new(
            id,
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = options.clone();
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        ))
    }));
    let old_call = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolCall(ToolCallPayload::new(
            "old-tool",
            serde_json::json!({"already_completed":true}),
        )),
        BlockMeta::default()
            .with_source("preserved-source")
            .with_provider_call_id("old-provider-call"),
    );
    let old_result = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolResult(ToolResultPayload::new(
            old_call.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({"saved_original":true})),
        )),
        BlockMeta::default(),
    );
    let original = vec![text_input("initial material"), old_call, old_result];
    let first = host
        .create(PreparedCreate::persistent(
            definition.clone(),
            Context::from_blocks(original.clone()).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(first.definition(), definition);
    assert_eq!(requests.lock().unwrap().len(), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    let first_request = SubmitRequest {
        request_key: "reused-key".into(),
        blocks: vec![text_input("T1 input")],
    };
    let t1 = first.submit(first_request.clone()).unwrap();
    let first_result = first.wait(&t1.work, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(first_result.state, WorkState::Finished(ref result)
        if matches!(&**result, WorkResult::Run(TurnResult::Completed { .. }))));
    assert!(matches!(
        first.wait_saved().await.unwrap(),
        SaveStatus::Saved { revision: 2, .. }
    ));
    let saved = first.read_materials().unwrap().context;
    assert_eq!(&saved.blocks()[..original.len()], original.as_slice());
    let old_target = first.target();
    host.close(&old_target, ClosePolicy::Save).await.unwrap();
    let reopened = host
        .open(old_target.conversation_id().clone())
        .await
        .unwrap();
    assert_ne!(reopened.target(), old_target);
    assert_eq!(reopened.definition(), definition);
    assert_eq!(
        reopened.read_materials().unwrap().context.blocks(),
        saved.blocks()
    );
    assert!(matches!(
        reopened.observe(&t1.work),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        reopened.cancel(&t1.work),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        reopened.wait(&t1.work, Duration::ZERO).await,
        Err(SessionError::NotFound { .. })
    ));
    // The old request table is absent: its original blocks now fail material
    // identity validation, rather than returning a retained receipt.
    assert!(matches!(
        reopened.submit(first_request).unwrap_err().reason,
        super::ApplicationError::Session(SessionError::ContextEdit(_))
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    let t2 = reopened
        .submit(SubmitRequest {
            request_key: "reused-key".into(),
            blocks: vec![text_input("T2 input")],
        })
        .unwrap();
    assert_ne!(t2.work.turn_id, t1.work.turn_id);
    assert_eq!(t2.work.conversation_id, t1.work.conversation_id);
    assert!(
        matches!(reopened.wait(&t2.work, Duration::from_secs(5)).await.unwrap().state,
        WorkState::Finished(ref result) if matches!(&**result, WorkResult::Run(TurnResult::Completed { .. })))
    );
    assert!(matches!(
        reopened.wait_saved().await.unwrap(),
        SaveStatus::Saved { revision: 3, .. }
    ));
    let final_context = reopened.read_materials().unwrap().context;
    assert_eq!(
        &final_context.blocks()[..saved.blocks().len()],
        saved.blocks()
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.model.0 == definition.model)
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(assemblies.load(Ordering::SeqCst), 2);
    let listing = host.list().await.unwrap();
    assert_eq!(listing[0].record.as_ref().unwrap().definition, definition);
    assert_eq!(listing[0].record.as_ref().unwrap().revision, 3);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}
