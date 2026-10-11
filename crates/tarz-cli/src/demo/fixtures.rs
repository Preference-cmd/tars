use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockMeta, CallControl, ContentPart, Context, ContextBlock, ModelGateway,
    ModelInvokeError, ModelItem, ModelOutput, ModelRef, ModelRequest, ModelResponse,
    ModelStopReason, TextPayload, Tool, ToolCallContext, ToolCallDraft, ToolDefinition, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use causa::runtime::{ToolExecutor, TurnRunOptions, TurnRunner, new_block_id};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tarz_core::session::{Session, SessionConfig, WorkRef};

#[derive(Default)]
pub(super) struct Counters {
    pub models: AtomicUsize,
    pub tools: AtomicUsize,
}

pub(super) fn input(text: &str) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::Parts(vec![ContentPart::Text(TextPayload::new(text))]),
        BlockMeta {
            source: Some("user".into()),
            ..Default::default()
        },
    )
}

pub(super) fn session(context: Context, counters: Arc<Counters>) -> Session {
    Session::new(
        "tarz-demo".into(),
        context,
        move |work, _control| {
            let counters = counters.clone();
            async move {
                let tool: Arc<dyn Tool> = Arc::new(LocalTool {
                    work: work.clone(),
                    counters: counters.clone(),
                });
                let runner = Arc::new(TurnRunner::new(
                    Arc::new(ScriptedGateway { counters }),
                    Arc::new(ToolExecutor::new(vec![tool], Default::default())?),
                ));
                Ok((runner, TurnRunOptions::new(ModelRef::new("offline-demo"))))
            }
        },
        SessionConfig {
            retained_work_capacity: 4,
            work_timeout: Some(Duration::from_secs(10)),
        },
    )
}

struct ScriptedGateway {
    counters: Arc<Counters>,
}
#[async_trait]
impl ModelGateway for ScriptedGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.counters.models.fetch_add(1, Ordering::SeqCst);
        let has_result = request
            .frame
            .blocks
            .iter()
            .any(|block| matches!(block.content(), BlockContent::ToolResult(_)));
        Ok(ModelOutput {
            response: ModelResponse {
                items: if has_result {
                    vec![ModelItem::text(format!(
                        "Offline result: {} material blocks.",
                        request.frame.blocks.len()
                    ))]
                } else {
                    vec![ModelItem::tool_call(ToolCallDraft {
                        tool_name: "demo_status".into(),
                        arguments: serde_json::json!({"work": request.invocation_id.turn_id.0}),
                        provider_call_id: None,
                    })]
                },
            },
            usage: None,
            stop_reason: if has_result {
                ModelStopReason::EndTurn
            } else {
                ModelStopReason::ToolUse
            },
            raw_stop_reason: None,
            reasoning: None,
        })
    }
}
struct LocalTool {
    work: WorkRef,
    counters: Arc<Counters>,
}
#[async_trait]
impl Tool for LocalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "demo_status".into(),
            description: "Return a local, deterministic demonstration result".into(),
            parameters: serde_json::json!({"type":"object","properties":{"work":{"type":"string"}},"required":["work"]}),
        }
    }
    async fn execute(
        &self,
        context: &ToolCallContext,
        _control: &CallControl,
    ) -> ToolResultPayload {
        self.counters.tools.fetch_add(1, Ordering::SeqCst);
        let matches_work =
            context.input.arguments["work"].as_str() == Some(self.work.turn_id.0.as_str());
        ToolResultPayload {
            call_block_id: context.call_block_id,
            status: if matches_work {
                ToolResultStatus::Succeeded
            } else {
                ToolResultStatus::Failed
            },
            output: ToolOutput::new(
                serde_json::json!({"offline":true,"work":self.work.turn_id.0,"scope_matches":matches_work}),
            ),
            media: vec![],
            notes: vec![],
        }
    }
}
