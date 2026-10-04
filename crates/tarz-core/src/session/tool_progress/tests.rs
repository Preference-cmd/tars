use super::*;
use crate::session::{
    ProgressUpdate, Session, SessionConfig, SubmitRequest, WorkRef, WorkResult, WorkState,
};
use causa::kernel::{
    BlockContent, BlockMeta, ContextBlock, InvocationId, RoundId, ToolBatch, ToolCallContext,
    ToolCallPayload, ToolOutput, ToolResultPayload, ToolResultStatus, TurnId,
};
use causa::runtime::{RunEvent, TurnRunOptions, new_block_id};
use std::sync::{Arc, Mutex, atomic::AtomicUsize};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

fn fixture() -> (
    ToolReporter,
    InvocationId,
    broadcast::Receiver<Arc<crate::session::WorkProgress>>,
) {
    let work = WorkRef {
        conversation_id: "tool-observation".into(),
        turn_id: TurnId::new("work-1"),
    };
    let invocation = InvocationId {
        turn_id: work.turn_id.clone(),
        round_id: RoundId(0),
    };
    let (sender, receiver) = broadcast::channel(2);
    (ToolReporter::new(work, sender), invocation, receiver)
}

fn call() -> ToolCallContext {
    ToolCallContext::from_declaration(
        new_block_id(),
        &ToolCallPayload {
            tool_name: "read_file".into(),
            arguments: serde_json::json!({"path":"source.rs"}),
        },
    )
}

fn ready(
    reporter: &ToolReporter,
    invocation: &InvocationId,
    calls: Vec<ToolCallContext>,
) -> ToolBatch {
    let batch = ToolBatch::new(calls).unwrap();
    reporter.observe(&RunEvent::ToolBatchReady {
        invocation_id: invocation,
        batch: &batch,
    });
    batch
}

#[test]
fn reliable_snapshot_survives_lost_events_and_tracks_actual_commit() {
    let (reporter, invocation, mut receiver) = fixture();
    let call = call();
    let mut batch = ready(&reporter, &invocation, vec![call.clone()]);
    reporter.running(&call);
    reporter.returned(&call, ToolResultStatus::Succeeded);
    let payload = ToolResultPayload {
        call_block_id: call.call_block_id,
        status: ToolResultStatus::Succeeded,
        output: ToolOutput::new(serde_json::json!({"text":"actual file bytes"})),
        media: Vec::new(),
        notes: Vec::new(),
    };
    batch
        .resolve_at(0, new_block_id(), payload.clone())
        .unwrap();
    reporter.observe(&RunEvent::ToolBatchReturned {
        invocation_id: &invocation,
        batch: &batch,
        result: Ok(()),
    });
    assert_eq!(reporter.snapshot().calls[0].phase, ToolPhase::Returned);
    let blocks = [ContextBlock::new(
        new_block_id(),
        BlockContent::ToolResult(payload),
        BlockMeta::default(),
    )];
    reporter.observe(&RunEvent::BlocksCommitted {
        invocation_id: &invocation,
        blocks: &blocks,
    });
    assert!(matches!(
        receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    let snapshot = reporter.snapshot();
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].phase, ToolPhase::Committed);
    assert_eq!(snapshot.calls[0].status, Some(ToolResultStatus::Succeeded));
    reporter.freeze();
    reporter.running(&call);
    assert_eq!(snapshot, reporter.snapshot());
    // Facts are borrowed, not taken: the original U still retains its result.
    assert_eq!(batch.results().len(), 1);
}

#[test]
fn controlled_unknown_and_pending_win_over_late_blocking_callbacks() {
    let (reporter, invocation, _) = fixture();
    let started = call();
    let unstarted = call();
    let mut batch = ready(
        &reporter,
        &invocation,
        vec![started.clone(), unstarted.clone()],
    );
    reporter.running(&started);
    batch
        .resolve_at(
            0,
            new_block_id(),
            ToolResultPayload {
                call_block_id: started.call_block_id,
                status: ToolResultStatus::UnknownOutcome,
                output: ToolOutput::new(serde_json::json!({"error":"cancelled before acceptance"})),
                media: Vec::new(),
                notes: Vec::new(),
            },
        )
        .unwrap();
    reporter.observe(&RunEvent::ToolBatchReturned {
        invocation_id: &invocation,
        batch: &batch,
        result: Ok(()),
    });
    reporter.returned(&started, ToolResultStatus::Succeeded);
    reporter.running(&unstarted);
    let snapshot = reporter.snapshot();
    assert_eq!(
        snapshot.calls[0].status,
        Some(ToolResultStatus::UnknownOutcome)
    );
    assert_eq!(snapshot.calls[1].phase, ToolPhase::NotStarted);
    assert_eq!(batch.results().len(), 1);
    assert_eq!(batch.calls().len(), 1);
}

#[test]
fn identical_wrapper_and_batch_result_still_seals_authoritative_facts() {
    let (reporter, invocation, _) = fixture();
    let call = call();
    let mut batch = ready(&reporter, &invocation, vec![call.clone()]);
    reporter.returned(&call, ToolResultStatus::Failed);
    batch
        .resolve_at(
            0,
            new_block_id(),
            ToolResultPayload {
                call_block_id: call.call_block_id,
                status: ToolResultStatus::Failed,
                output: ToolOutput::new(serde_json::json!({})),
                media: vec![],
                notes: vec![],
            },
        )
        .unwrap();
    reporter.observe(&RunEvent::ToolBatchReturned {
        invocation_id: &invocation,
        batch: &batch,
        result: Ok(()),
    });
    reporter.returned(&call, ToolResultStatus::Succeeded);
    assert_eq!(
        reporter.snapshot().calls[0].status,
        Some(ToolResultStatus::Failed)
    );
}

#[test]
fn drained_wrapper_start_facts_refine_unknown_without_changing_original_results() {
    for committed in [false, true] {
        let (reporter, invocation, _) = fixture();
        let waiting = call();
        let late_start = call();
        let uninstrumented = call();
        let mut batch = ready(
            &reporter,
            &invocation,
            vec![waiting.clone(), late_start.clone(), uninstrumented.clone()],
        );
        reporter.waiting(&waiting);
        reporter.waiting(&late_start);
        let mut blocks = Vec::new();
        for (index, call) in [&waiting, &late_start, &uninstrumented]
            .into_iter()
            .enumerate()
        {
            let payload = ToolResultPayload {
                call_block_id: call.call_block_id,
                status: ToolResultStatus::UnknownOutcome,
                output: ToolOutput::new(serde_json::json!({"runtime":"conservative unknown"})),
                media: Vec::new(),
                notes: Vec::new(),
            };
            let id = new_block_id();
            blocks.push(ContextBlock::new(
                id,
                BlockContent::ToolResult(payload.clone()),
                BlockMeta::default(),
            ));
            batch.resolve_at(index, id, payload).unwrap();
        }
        reporter.observe(&RunEvent::ToolBatchReturned {
            invocation_id: &invocation,
            batch: &batch,
            result: Ok(()),
        });
        if committed {
            reporter.observe(&RunEvent::BlocksCommitted {
                invocation_id: &invocation,
                blocks: &blocks,
            });
        }
        // A queued blocking job can enter after the conservative return. Its
        // actual start still matters even though it cannot overwrite that result.
        reporter.running(&late_start);
        reporter.returned(&late_start, ToolResultStatus::Succeeded);
        reporter.freeze();
        let snapshot = reporter.snapshot();
        assert!(snapshot.calls[0].operation_not_started);
        assert!(!snapshot.calls[1].operation_not_started);
        assert!(
            !snapshot.calls[2].operation_not_started,
            "absence of an instrumented wrapper is not proof of no execution"
        );
        assert_eq!(
            snapshot.calls[0].phase,
            if committed {
                ToolPhase::Committed
            } else {
                ToolPhase::NotStarted
            }
        );
        assert_eq!(
            snapshot.calls[1].phase,
            if committed {
                ToolPhase::Committed
            } else {
                ToolPhase::Unknown
            }
        );
        assert!(
            snapshot
                .calls
                .iter()
                .all(|row| row.status == Some(ToolResultStatus::UnknownOutcome))
        );
        assert!(
            batch
                .results()
                .iter()
                .all(|entry| entry.result().unwrap().1.status == ToolResultStatus::UnknownOutcome)
        );
    }
}

#[test]
fn bounded_summaries_keep_duplicate_declarations_and_abnormal_states_distinct() {
    let (reporter, invocation, _) = fixture();
    let mut calls = (0..130).map(|_| call()).collect::<Vec<_>>();
    calls[0].input.tool_name = "🦀".repeat(100);
    calls[0].input.arguments = serde_json::json!({"unicode":"🦀\n".repeat(100_000)});
    let batch = ready(&reporter, &invocation, calls.clone());
    reporter.running(&calls[0]);
    reporter.freeze();
    let snapshot = reporter.snapshot();
    assert_eq!(snapshot.calls.len(), 128);
    assert!(snapshot.truncated);
    assert!(snapshot.calls[0].tool_name.len() <= 128);
    assert!(snapshot.calls[0].arguments_summary.len() <= 256);
    assert_eq!(snapshot.calls[0].phase, ToolPhase::Unknown);
    assert_eq!(snapshot.calls[1].phase, ToolPhase::NotStarted);
    assert_ne!(
        snapshot.calls[1].call_block_id,
        snapshot.calls[2].call_block_id
    );
    assert_eq!(
        snapshot.calls[1].arguments_summary,
        snapshot.calls[2].arguments_summary
    );
    assert_eq!(batch.calls().len(), 130);
}

#[test]
fn tool_events_preserve_work_invocation_and_declaration() {
    let (reporter, mut invocation, mut receiver) = fixture();
    invocation.round_id = RoundId(3);
    let call = call();
    let _batch = ready(&reporter, &invocation, vec![call.clone()]);
    let event = receiver.try_recv().unwrap();
    assert_eq!(event.work.turn_id, invocation.turn_id);
    assert_eq!(event.invocation_id, invocation);
    let ProgressUpdate::Tool(tool) = &event.update else {
        panic!("expected tool event")
    };
    assert_eq!(tool.call_block_id, call.call_block_id);
    assert_eq!(tool.invocation_id, invocation);
    assert_eq!(tool.phase, ToolPhase::Waiting);
}

#[tokio::test]
async fn dropped_blocking_handle_remains_owned_until_actual_completion() {
    let (reporter, _, _) = fixture();
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let (release, wait) = std::sync::mpsc::channel();
    drop(reporter.spawn_blocking(move || {
        signal.notify_one();
        wait.recv().unwrap();
    }));
    entered.notified().await;
    let drained = reporter.drain();
    tokio::pin!(drained);
    assert!(futures_util::poll!(&mut drained).is_pending());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), drained)
        .await
        .unwrap();
}

async fn supervised_terminal(fault: bool) {
    use crate::session::progress_tests::{Ending, ScriptGateway, runner};
    let entered = Arc::new(Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    let receiver = Arc::new(Mutex::new(Some(wait)));
    let assemble_entered = entered.clone();
    let mut owner = Session::new_with_tools(
        "supervised-tools".into(),
        causa::kernel::Context::new(),
        move |_, _, reporter| {
            let wait = receiver.lock().unwrap().take().unwrap();
            let signal = assemble_entered.clone();
            async move {
                drop(reporter.spawn_blocking(move || {
                    signal.notify_one();
                    wait.recv().unwrap();
                }));
                assert!(
                    !fault,
                    "simulated assembly unwind with owned filesystem work"
                );
                Ok((
                    runner(ScriptGateway {
                        deltas: vec![],
                        ending: Ending::Complete,
                        release: None,
                        calls: Arc::new(AtomicUsize::new(0)),
                    }),
                    TurnRunOptions::new(causa::kernel::ModelRef::new("fixture")),
                ))
            }
        },
        SessionConfig {
            retained_work_capacity: 1,
            work_timeout: None,
        },
    );
    let handle = owner.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "one".into(),
            blocks: vec![],
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    assert!(!matches!(
        handle.observe(&receipt.work).unwrap().state,
        WorkState::Finished(_)
    ));
    let shutdown = owner.shutdown();
    tokio::pin!(shutdown);
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .unwrap();
    let observation = handle.observe(&receipt.work).unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("work must be settled")
    };
    if fault {
        assert!(matches!(result.as_ref(), WorkResult::Faulted(_)));
    }
    assert!(observation.tools.calls.is_empty());
}

#[tokio::test]
async fn normal_terminal_and_close_wait_for_owned_filesystem_jobs() {
    supervised_terminal(false).await;
}

#[tokio::test]
async fn fault_terminal_and_close_wait_for_owned_filesystem_jobs() {
    supervised_terminal(true).await;
}
