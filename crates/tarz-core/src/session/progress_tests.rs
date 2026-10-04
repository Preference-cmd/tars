use super::*;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelInvokeErrorKind, ModelOutput,
    ModelRef, ModelRequest, ModelResponse, ModelStopReason, ModelStream, StreamDelta, TextPayload,
};
use causa::runtime::{
    RunEvent, RunObserver, ToolExecutor, ToolExecutorOptions, TurnResult, TurnRunOptions,
    TurnRunner,
};
use futures_util::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

#[derive(Clone, Copy)]
pub(crate) enum Ending {
    Complete,
    Error,
    Eof,
}

pub(crate) struct ScriptGateway {
    pub deltas: Vec<StreamDelta>,
    pub ending: Ending,
    pub release: Option<Arc<Notify>>,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelGateway for ScriptGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        panic!("Session must use gateway.stream through run_streaming");
    }
    async fn stream(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let release = self.release.clone();
        let ending = self.ending;
        Ok(Box::pin(
            futures_util::stream::iter(self.deltas.clone()).chain(
                futures_util::stream::once(async move {
                    if let Some(release) = release {
                        release.notified().await;
                    }
                    match ending {
                        Ending::Complete => Some(StreamDelta::Done {
                            stop_reason: ModelStopReason::EndTurn,
                            final_output: final_output(),
                        }),
                        Ending::Error => Some(StreamDelta::Error {
                            kind: ModelInvokeErrorKind::Permanent,
                            message: "script stream failure".into(),
                        }),
                        Ending::Eof => None,
                    }
                })
                .filter_map(futures_util::future::ready),
            ),
        ))
    }
}

pub(crate) fn final_output() -> ModelOutput {
    ModelOutput {
        response: ModelResponse {
            text: TextPayload::new("authoritative saved output"),
            tool_calls: vec![],
        },
        usage: None,
        stop_reason: ModelStopReason::EndTurn,
        reasoning: None,
    }
}
pub(crate) fn runner(gateway: ScriptGateway) -> Arc<TurnRunner> {
    Arc::new(TurnRunner::new(
        Arc::new(gateway),
        Arc::new(ToolExecutor::new(vec![], ToolExecutorOptions::default()).unwrap()),
    ))
}
fn session(gateway: ScriptGateway, previous: Option<Arc<RunObserver>>) -> Session {
    let runner = runner(gateway);
    Session::new(
        "streaming".into(),
        Context::new(),
        move |_, _| {
            let runner = runner.clone();
            let observer = previous.clone();
            async move {
                let mut options = TurnRunOptions::new(ModelRef::new("script"));
                options.observer = observer;
                Ok((runner, options))
            }
        },
        SessionConfig {
            retained_work_capacity: 8,
            work_timeout: None,
        },
    )
}
pub(crate) fn request(key: &str) -> SubmitRequest {
    SubmitRequest {
        request_key: key.into(),
        blocks: vec![],
    }
}
async fn recv(receiver: &mut broadcast::Receiver<Arc<WorkProgress>>) -> Arc<WorkProgress> {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn session_progress_precedes_terminal_and_preserves_original_observer() {
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let mut owner = session(
        ScriptGateway {
            deltas: vec![
                StreamDelta::TextDelta {
                    delta: "temporary text".into(),
                },
                StreamDelta::ReasoningDelta {
                    delta: "private reasoning".into(),
                },
                StreamDelta::ToolCallDelta {
                    call_index: 0,
                    provider_call_id: None,
                    name_delta: None,
                    arguments_delta: Some("private arguments".into()),
                },
            ],
            ending: Ending::Complete,
            release: Some(release.clone()),
            calls: calls.clone(),
        },
        Some({
            let count = observed.clone();
            Arc::new(move |event| {
                if matches!(event, RunEvent::ModelDelta { .. }) {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            })
        }),
    );
    let handle = owner.handle();
    let mut progress = handle.subscribe_progress();
    let receipt = handle.submit(request("once")).unwrap();
    let started = recv(&mut progress).await;
    let text = recv(&mut progress).await;
    assert_eq!(started.work, receipt.work);
    assert_eq!(started.invocation_id.turn_id, receipt.work.turn_id);
    assert_eq!(started.update, ProgressUpdate::ModelStarted);
    assert_eq!(text.invocation_id, started.invocation_id);
    assert_eq!(
        text.update,
        ProgressUpdate::Text {
            delta: "temporary text".into(),
            truncated: false
        }
    );
    assert!(matches!(
        handle.observe(&receipt.work).unwrap().state,
        WorkState::Running
    ));
    assert!(matches!(
        progress.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(observed.load(Ordering::SeqCst), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    let result = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(result.state,WorkState::Finished(result) if matches!(&*result,WorkResult::Run(TurnResult::Completed{final_output}) if final_output.response.text.0=="authoritative saved output"))
    );
    assert_eq!(
        recv(&mut progress).await.update,
        ProgressUpdate::ModelOutputReady
    );
    let material = owner
        .with_context(|context| serde_json::to_string(context).unwrap())
        .unwrap();
    assert!(material.contains("authoritative saved output"));
    assert!(!material.contains("temporary text"));
    let mut late = handle.subscribe_progress();
    assert!(matches!(
        late.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn text_progress_is_bounded_at_utf8_boundary() {
    let delta = format!("{}🦀extra", "a".repeat(16 * 1024 - 1));
    let release = Arc::new(Notify::new());
    let mut owner = session(
        ScriptGateway {
            deltas: vec![StreamDelta::TextDelta { delta }],
            ending: Ending::Complete,
            release: Some(release.clone()),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        None,
    );
    let handle = owner.handle();
    let mut progress = handle.subscribe_progress();
    let receipt = handle.submit(request("bounded")).unwrap();
    recv(&mut progress).await;
    match &recv(&mut progress).await.update {
        ProgressUpdate::Text { delta, truncated } => {
            assert_eq!(delta.len(), 16 * 1024 - 1);
            assert!(*truncated);
        }
        _ => panic!("expected text"),
    }
    release.notify_one();
    assert!(matches!(
        handle
            .wait(&receipt.work, Duration::from_secs(3))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    owner.shutdown().await;
}

#[tokio::test]
async fn lagged_and_dropped_progress_receivers_do_not_delay_reliable_result() {
    let mut owner = session(
        ScriptGateway {
            deltas: (0..256)
                .map(|i| StreamDelta::TextDelta {
                    delta: i.to_string(),
                })
                .collect(),
            ending: Ending::Complete,
            release: None,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        None,
    );
    let handle = owner.handle();
    let mut progress = handle.subscribe_progress();
    let receipt = handle.submit(request("lag")).unwrap();
    let result = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(matches!(result.state, WorkState::Finished(_)));
    assert_eq!(progress.len(), 258); // next receive reports exact missed observations
    assert!(matches!(
        progress.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(130))
    ));
    assert_eq!(progress.len(), 128);
    drop(progress);
    assert!(matches!(
        handle.observe(&receipt.work).unwrap().state,
        WorkState::Finished(_)
    ));
    assert_eq!(
        owner
            .with_context(|context| context.blocks().len())
            .unwrap(),
        1
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn no_progress_subscriber_retains_no_preview_but_original_observer_still_runs() {
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    let mut owner = session(
        ScriptGateway {
            deltas: (0..256)
                .map(|_| StreamDelta::TextDelta {
                    delta: "private".repeat(100),
                })
                .collect(),
            ending: Ending::Complete,
            release: None,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        Some(Arc::new(move |event| {
            if matches!(event, RunEvent::ModelDelta { .. }) {
                count.fetch_add(1, Ordering::SeqCst);
            }
        })),
    );
    let handle = owner.handle();
    let receipt = handle.submit(request("unobserved")).unwrap();
    handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), 256);
    assert_eq!(owner.shared.progress.len(), 0);
    owner.shutdown().await;
}

#[tokio::test]
async fn cancellation_error_and_missing_terminal_do_not_commit_preview_material() {
    for ending in [Ending::Complete, Ending::Error, Ending::Eof] {
        let release = Arc::new(Notify::new());
        let mut owner = session(
            ScriptGateway {
                deltas: vec![StreamDelta::TextDelta {
                    delta: "partial-preview".into(),
                }],
                ending,
                release: Some(release.clone()),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            None,
        );
        let handle = owner.handle();
        let mut progress = handle.subscribe_progress();
        let receipt = handle.submit(request("stopped")).unwrap();
        recv(&mut progress).await;
        recv(&mut progress).await;
        if matches!(ending, Ending::Complete) {
            handle.cancel(&receipt.work).unwrap();
        } else {
            release.notify_one();
        }
        let observation = handle
            .wait(&receipt.work, Duration::from_secs(3))
            .await
            .unwrap();
        assert!(
            matches!(observation.state,WorkState::Finished(result) if matches!(&*result,WorkResult::Run(TurnResult::Interrupted{..})))
        );
        assert!(
            owner
                .with_context(|context| context.blocks().is_empty())
                .unwrap()
        );
        assert!(matches!(
            progress.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        owner.shutdown().await;
    }
}

struct RoundGateway;
#[async_trait::async_trait]
impl ModelGateway for RoundGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        panic!("multi-round execution must stream")
    }
    async fn stream(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        let mut output = final_output();
        if request.invocation_id.round_id.0 == 0 {
            output
                .response
                .tool_calls
                .push(causa::kernel::ToolCallDraft {
                    tool_name: "round_tool".into(),
                    arguments: serde_json::json!({}),
                    provider_call_id: Some("round-call".into()),
                });
            output.stop_reason = ModelStopReason::ToolUse;
        }
        Ok(Box::pin(futures_util::stream::iter(vec![
            StreamDelta::TextDelta {
                delta: format!("round {} preview", request.invocation_id.round_id.0),
            },
            StreamDelta::Done {
                stop_reason: output.stop_reason,
                final_output: output,
            },
        ])))
    }
}
struct RoundTool(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl causa::kernel::Tool for RoundTool {
    fn definition(&self) -> causa::kernel::ToolDefinition {
        causa::kernel::ToolDefinition {
            name: "round_tool".into(),
            description: "round fixture".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }
    async fn execute(
        &self,
        call: &causa::kernel::ToolCallContext,
        _: &CallControl,
    ) -> causa::kernel::ToolResultPayload {
        self.0.fetch_add(1, Ordering::SeqCst);
        causa::kernel::ToolResultPayload {
            call_block_id: call.call_block_id,
            status: causa::kernel::ToolResultStatus::Succeeded,
            output: causa::kernel::ToolOutput::new(serde_json::json!({})),
            media: vec![],
            notes: vec![],
        }
    }
}

#[tokio::test]
async fn model_progress_tracks_each_invocation_in_a_multi_round_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(TurnRunner::new(
        Arc::new(RoundGateway),
        Arc::new(
            ToolExecutor::new(
                vec![Arc::new(RoundTool(calls.clone()))],
                ToolExecutorOptions::default(),
            )
            .unwrap(),
        ),
    ));
    let mut owner = Session::new(
        "rounds".into(),
        Context::new(),
        move |_, _| {
            let runner = runner.clone();
            async move { Ok((runner, TurnRunOptions::new(ModelRef::new("rounds")))) }
        },
        SessionConfig {
            retained_work_capacity: 8,
            work_timeout: None,
        },
    );
    let handle = owner.handle();
    let mut progress = handle.subscribe_progress();
    let receipt = handle.submit(request("rounds")).unwrap();
    assert!(matches!(
        handle
            .wait(&receipt.work, Duration::from_secs(3))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    let mut started = Vec::new();
    let mut output_rounds = Vec::new();
    let mut text_rounds = Vec::new();
    while let Ok(item) = progress.try_recv() {
        assert_eq!(item.work, receipt.work);
        assert_eq!(item.invocation_id.turn_id, receipt.work.turn_id);
        match &item.update {
            ProgressUpdate::ModelStarted => started.push(item.invocation_id.round_id.0),
            ProgressUpdate::ModelOutputReady => output_rounds.push(item.invocation_id.round_id.0),
            ProgressUpdate::Text { .. } => text_rounds.push(item.invocation_id.round_id.0),
        }
    }
    assert_eq!(started, [0, 1]);
    assert_eq!(text_rounds, [0, 1]);
    assert_eq!(output_rounds, [0, 1]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    owner.shutdown().await;
}
