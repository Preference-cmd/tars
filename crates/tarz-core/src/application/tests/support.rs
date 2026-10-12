pub(super) use super::super::*;
pub(super) use crate::definition::{CredentialRef, Provider, SessionDefinition};
pub(super) use crate::session::{Session, SubmitRequest, WorkResult, WorkState};
pub(super) use crate::store::{CommitStage, FaultAction, FaultHook};
pub(super) use causa::kernel::{BlockContent, BlockId, BlockMeta, Context, ContextBlock};
pub(super) use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnRunner};
pub(super) use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
pub(super) use std::time::Duration;
pub(super) use tokio::sync::Notify;

pub(super) fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARZ_UNUSED_FIXTURE".into()),
        work_timeout_secs: None,
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        max_model_attempts: None,
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
    }
}
pub(super) fn request(key: &str) -> SubmitRequest {
    SubmitRequest {
        request_key: key.into(),
        blocks: vec![ContextBlock::new(
            BlockId::new(uuid::Uuid::now_v7()),
            BlockContent::Parts(vec![]),
            BlockMeta::default(),
        )],
    }
}
pub(super) struct Gateway {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) entered: Option<Arc<Notify>>,
    pub(super) release: Option<Arc<Notify>>,
    pub(super) panic: bool,
}
#[async_trait::async_trait]
impl causa::kernel::ModelGateway for Gateway {
    async fn invoke(
        &self,
        _: &causa::kernel::ModelRequest,
        _: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "injected worker panic");
        if let Some(entered) = &self.entered {
            entered.notify_one();
        }
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(causa::kernel::ModelOutput::new(
            causa::kernel::ModelResponse {
                items: vec![causa::kernel::ModelItem::text("completed original")],
            },
            causa::kernel::ModelStopReason::EndTurn,
        ))
    }
}
pub(super) fn fixture(host: &SessionHost, gateway: Gateway) -> Arc<AtomicUsize> {
    let assemblies = Arc::new(AtomicUsize::new(0));
    let count = assemblies.clone();
    let runner = Arc::new(TurnRunner::new(
        Arc::new(gateway),
        Arc::new(ToolExecutor::new(vec![], ToolExecutorOptions::default()).unwrap()),
    ));
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        count.fetch_add(1, Ordering::SeqCst);
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
    assemblies
}
pub(super) fn normal_fixture(host: &SessionHost) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    fixture(
        host,
        Gateway {
            calls: calls.clone(),
            entered: None,
            release: None,
            panic: false,
        },
    );
    calls
}
pub(super) struct Gate {
    pub(super) entered: Notify,
    pub(super) released: Mutex<bool>,
    pub(super) release: Condvar,
}
impl Gate {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    pub(super) fn hook(self: &Arc<Self>, stage: CommitStage) -> FaultHook {
        let gate = self.clone();
        let hits = AtomicUsize::new(0);
        Arc::new(move |current| {
            if current == stage && hits.fetch_add(1, Ordering::SeqCst) == 0 {
                gate.entered.notify_one();
                let mut released = gate.released.lock().unwrap();
                while !*released {
                    released = gate.release.wait(released).unwrap();
                }
            }
            FaultAction::Continue
        })
    }
    pub(super) async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }
    pub(super) fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
pub(super) async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("managed state transition timed out");
}
pub(super) async fn finished(session: &ManagedSession, work: &crate::session::WorkRef) {
    assert!(matches!(
        session
            .wait(work, Duration::from_secs(5))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
}

pub(super) struct ToolGateway;
#[async_trait::async_trait]
impl causa::kernel::ModelGateway for ToolGateway {
    async fn invoke(
        &self,
        _: &causa::kernel::ModelRequest,
        _: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        Ok(causa::kernel::ModelOutput::new(
            causa::kernel::ModelResponse {
                items: vec![causa::kernel::ModelItem::tool_call(
                    causa::kernel::ToolCallDraft::new("fixture", serde_json::json!({})),
                )],
            },
            causa::kernel::ModelStopReason::ToolUse,
        ))
    }
}
pub(super) struct HeldTool {
    pub(super) entered: Arc<Notify>,
    pub(super) release: Arc<Notify>,
    pub(super) stopped: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl causa::kernel::Tool for HeldTool {
    fn definition(&self) -> causa::kernel::ToolDefinition {
        causa::kernel::ToolDefinition::new(
            "fixture",
            "fixture",
            serde_json::json!({"type":"object"}),
        )
    }
    async fn execute(
        &self,
        call: &causa::kernel::ToolCallContext,
        _: &causa::kernel::CallControl,
    ) -> causa::kernel::ToolResultPayload {
        struct Stopped(Arc<AtomicUsize>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _stopped = Stopped(self.stopped.clone());
        self.entered.notify_one();
        self.release.notified().await;
        causa::kernel::ToolResultPayload::new(
            call.call_block_id,
            causa::kernel::ToolResultStatus::Succeeded,
            causa::kernel::ToolOutput::new(serde_json::json!({"actual":true})),
        )
    }
}
pub(super) fn custom_runner(host: &SessionHost, runner: Arc<TurnRunner>) {
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
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
}

pub(super) struct CollisionBatch(pub(super) BlockId);
#[async_trait::async_trait]
impl causa::kernel::ToolBatchProcessor for CollisionBatch {
    async fn process(
        &self,
        batch: &mut causa::kernel::ToolBatch,
        _: &causa::kernel::ProcessorContext<'_>,
    ) -> Result<(), causa::kernel::ProcessorError> {
        let call = batch.calls()[0].call().clone();
        batch
            .resolve_at(0, self.0, {
                let mut result = causa::kernel::ToolResultPayload::new(
                    call.call_block_id,
                    causa::kernel::ToolResultStatus::Rejected,
                    causa::kernel::ToolOutput::new(serde_json::json!({"original":true})),
                );
                result.media = vec![causa::kernel::MediaRef::new(
                    "image/png",
                    "uncommitted original asset",
                )];
                result
            })
            .unwrap();
        Ok(())
    }
}
