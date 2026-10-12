#![allow(dead_code)]

pub mod execution;

use async_trait::async_trait;
use causa::{kernel::*, runtime::*};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tarz_core::session::*;

pub const BOUND: Duration = Duration::from_secs(5);
pub fn config(capacity: usize) -> SessionConfig {
    SessionConfig {
        retained_work_capacity: capacity,
        work_timeout: None,
    }
}
pub fn block(text: &str) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::Parts(vec![ContentPart::Text(TextPayload::new(text))]),
        BlockMeta::default()
            .with_source("user")
            .with_provider_call_id(format!("provenance:{text}")),
    )
}
pub fn request(key: &str, blocks: Vec<ContextBlock>) -> SubmitRequest {
    SubmitRequest {
        request_key: key.into(),
        blocks,
    }
}
pub fn final_output(text: &str) -> ModelOutput {
    ModelOutput::new(
        ModelResponse {
            items: vec![ModelItem::text(text)],
        },
        ModelStopReason::EndTurn,
    )
}
pub fn tool_output(names: &[&str]) -> ModelOutput {
    ModelOutput::new(
        ModelResponse {
            items: names
                .iter()
                .map(|name| {
                    ModelItem::tool_call(
                        ToolCallDraft::new(*name, serde_json::json!({"original":name}))
                            .with_provider_call_id(format!("provider:{name}")),
                    )
                })
                .collect(),
        },
        ModelStopReason::ToolUse,
    )
}
pub struct Gateway {
    outputs: Mutex<VecDeque<Result<ModelOutput, ModelInvokeError>>>,
    pub requests: Mutex<Vec<ModelRequest>>,
}
impl Gateway {
    pub fn new(outputs: Vec<ModelOutput>) -> Arc<Self> {
        Self::results(outputs.into_iter().map(Ok).collect())
    }
    pub fn results(outputs: Vec<Result<ModelOutput, ModelInvokeError>>) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl ModelGateway for Gateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.requests.lock().unwrap().push(request.clone());
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call")
    }
}
pub fn runner(gateway: Arc<dyn ModelGateway>, tools: Vec<Arc<dyn Tool>>) -> Arc<TurnRunner> {
    Arc::new(TurnRunner::new(
        gateway,
        Arc::new(ToolExecutor::new(tools, ToolExecutorOptions::default()).unwrap()),
    ))
}
pub fn simple_session(context: Context, capacity: usize, gateway: Arc<Gateway>) -> Session {
    let runner = runner(gateway, vec![]);
    Session::new(
        "conversation".into(),
        context,
        move |_, _| {
            let runner = runner.clone();
            async move { Ok((runner, TurnRunOptions::new(ModelRef::new("fixture")))) }
        },
        config(capacity),
    )
}
pub async fn finished(handle: &SessionHandle, work: &WorkRef) -> Arc<WorkResult> {
    let observation = tokio::time::timeout(BOUND, handle.wait(work, BOUND))
        .await
        .expect("work hung")
        .unwrap();
    assert_eq!(&observation.work, work);
    match observation.state {
        WorkState::Finished(result) => result,
        state => panic!("not finished: {state:?}"),
    }
}
pub fn completed(result: &WorkResult, text: &str) {
    let WorkResult::Run(TurnResult::Completed { final_output }) = result else {
        panic!("unexpected result: {result:?}")
    };
    assert_eq!(final_output.response.text(), text);
}
pub fn context(session: &mut Session) -> Vec<ContextBlock> {
    session
        .with_context(|context| context.blocks().to_vec())
        .unwrap()
}
