//! Post-compaction `<task-notification>` injection tests for `CompactionPreparer`.

use super::super::estimate::{UsageTracker, UsageTrackingGateway};
use super::super::preparer::{CompactionPreparer, TransientTrimPreparer};
use super::super::trim::ProtectedInputs;
use super::super::{CompactionRuntime, summary};
use super::{ToolThenDone, big_block, definition, message_text, tool_change};
use crate::events::{
    AgentNotificationPayload, DEFAULT_NOTIFICATION_INLINE_MAX_BYTES, EventDeliveryState,
    EventPriority, EventSourceKind, InboxDrainHandle, InboxEvent, OperationStatus,
    TASK_NOTIFICATION_SOURCE, WakePolicy,
};
use crate::session::{Session, SessionConfig, SubmitRequest, WorkResult, WorkState};
use causa::kernel::{
    ArtifactStore, BlockContent, BlockId, CallControl, Context, ContextBlock, ModelGateway, Role,
    Tool, ToolCallContext, ToolDefinition, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnLimits, TurnRunOptions, TurnRunner};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

#[tokio::test]
async fn compaction_preparer_injects_task_notification_after_compaction_and_never_reinjects() {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let summary_inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        summary_inputs: summary_inputs.clone(),
        tool_call_first: true,
        cache_read_tokens: None,
    });
    let tracker = Arc::new(UsageTracker::default());

    let mut context = Context::new();
    context
        .edit()
        .append([tool_change("alpha"), big_block(3_000)])
        .commit()
        .unwrap();

    let payload_json = serde_json::to_string(&AgentNotificationPayload {
        operation_id: "op-bg-1".into(),
        agent_id: "child-1".into(),
        agent_name: "explore".into(),
        status: OperationStatus::Completed,
        summary_text: "found matching symbol".into(),
        total_bytes: 21,
        truncated: false,
        error: None,
    })
    .unwrap();
    let inbox = Arc::new(InboxDrainHandle::with_initial(
        vec![InboxEvent {
            event_id: 42,
            session_id: "conversation".into(),
            target_agent_id: "main".into(),
            priority: EventPriority::Normal,
            source_kind: EventSourceKind::AgentOperation,
            source_id: "op-bg-1".into(),
            coalesce_key: None,
            wake_policy: WakePolicy::WakeIfIdle,
            payload_json,
            state: EventDeliveryState::Pending,
            claimed_op_id: None,
            created_at_ms: 1_000,
            delivered_at_ms: None,
        }],
        DEFAULT_NOTIFICATION_INLINE_MAX_BYTES,
    ));

    let tracked: Arc<dyn ModelGateway> =
        Arc::new(UsageTrackingGateway::new(gateway, tracker.clone()));
    let def = definition(Some(1_000), None);
    let runtime = CompactionRuntime::new(
        &def,
        &"conversation".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        tracked.clone(),
        tracker,
    )
    .unwrap();

    struct AlphaTool;
    #[async_trait::async_trait]
    impl Tool for AlphaTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "alpha",
                "alpha fixture",
                serde_json::json!({"type": "object"}),
            )
        }
        async fn execute(
            &self,
            call: &ToolCallContext,
            _control: &CallControl,
            _artifacts: Option<&dyn ArtifactStore>,
        ) -> ToolResultPayload {
            ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({"ok": true})),
            )
        }
    }

    let executor = Arc::new(
        ToolExecutor::new(vec![Arc::new(AlphaTool)], ToolExecutorOptions::default()).unwrap(),
    );
    let runner = Arc::new(TurnRunner::new(tracked, executor));
    let runtime_for_work = runtime.clone();
    let inbox_for_work = inbox.clone();

    let mut session = Session::new_with_tools(
        "conversation".into(),
        context,
        move |_work, _control, reporter| {
            let runner = runner.clone();
            let runtime = runtime_for_work.clone();
            let inbox = inbox_for_work.clone();
            async move {
                let options = TurnRunOptions::new(causa::kernel::ModelRef::new("fixture"))
                    .with_limits(TurnLimits::new(3, 8))
                    .with_preparer(Arc::new(
                        CompactionPreparer::new(runtime, reporter).with_inbox(inbox),
                    ));
                Ok((runner, options))
            }
        },
        SessionConfig {
            retained_work_capacity: 8,
            work_timeout: None,
        },
    )
    .with_compaction(runtime.clone());

    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "post-compact-inbox".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "continue",
            )],
        })
        .unwrap();

    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = &observation.state else {
        panic!("work unfinished");
    };
    assert!(matches!(
        &**result,
        WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));

    // Compaction ran on round 0 BEFORE notification injection, so summary input did NOT see the notification
    assert!(summary_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    let first_summary_input = { summary_inputs.lock().unwrap()[0].clone() };
    assert!(
        !first_summary_input
            .iter()
            .any(|b| b.meta().source.as_deref() == Some(TASK_NOTIFICATION_SOURCE))
    );

    let blocks = session.with_context(|ctx| ctx.blocks().to_vec()).unwrap();
    let notif_positions: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter_map(|(idx, b)| {
            (b.meta().source.as_deref() == Some(TASK_NOTIFICATION_SOURCE)).then_some(idx)
        })
        .collect();
    // Drained once on round 0, never re-injected on round 1
    assert_eq!(notif_positions.len(), 1);
    let notif_idx = notif_positions[0];
    let brief_idx = blocks
        .iter()
        .position(|b| b.meta().source.as_deref() == Some(summary::SUMMARY_SOURCE))
        .expect("compaction summary block present");
    assert!(notif_idx > brief_idx);

    let BlockContent::Message { role, .. } = blocks[notif_idx].content() else {
        panic!("expected message block");
    };
    assert_eq!(*role, Role::User);
    let text = message_text(&blocks[notif_idx]);
    assert!(text.contains("<task-notification"));
    assert!(text.contains("found matching symbol"));

    let consumed = inbox.consumed_snapshot();
    assert_eq!(consumed.delivered_ids, vec![42]);
    assert!(consumed.dropped_ids.is_empty());

    session.shutdown().await;
}

#[tokio::test]
async fn transient_trim_preparer_injects_late_arriving_event_between_rounds() {
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: true,
        cache_read_tokens: None,
    });
    let tracker = Arc::new(UsageTracker::default());
    let tracked: Arc<dyn ModelGateway> =
        Arc::new(UsageTrackingGateway::new(gateway, tracker.clone()));

    let mut context = Context::new();
    context
        .edit()
        .append([tool_change("alpha")])
        .commit()
        .unwrap();

    let inbox = Arc::new(InboxDrainHandle::new(DEFAULT_NOTIFICATION_INLINE_MAX_BYTES));

    struct LateEventTool {
        inbox: Arc<InboxDrainHandle>,
    }

    #[async_trait::async_trait]
    impl Tool for LateEventTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "alpha",
                "alpha late event",
                serde_json::json!({"type": "object"}),
            )
        }
        async fn execute(
            &self,
            call: &ToolCallContext,
            _control: &CallControl,
            _artifacts: Option<&dyn ArtifactStore>,
        ) -> ToolResultPayload {
            let payload_json = serde_json::to_string(&AgentNotificationPayload {
                operation_id: "op-late-1".into(),
                agent_id: "child-2".into(),
                agent_name: "general".into(),
                status: OperationStatus::Completed,
                summary_text: "late event arrived between rounds".into(),
                total_bytes: 33,
                truncated: false,
                error: None,
            })
            .unwrap();
            self.inbox.enqueue(InboxEvent {
                event_id: 99,
                session_id: "transient".into(),
                target_agent_id: "main".into(),
                priority: EventPriority::Urgent,
                source_kind: EventSourceKind::AgentOperation,
                source_id: "op-late-1".into(),
                coalesce_key: None,
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json,
                state: EventDeliveryState::Pending,
                claimed_op_id: None,
                created_at_ms: 2_000,
                delivered_at_ms: None,
            });
            ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({"ok": true})),
            )
        }
    }

    let executor = Arc::new(
        ToolExecutor::new(
            vec![Arc::new(LateEventTool {
                inbox: inbox.clone(),
            })],
            ToolExecutorOptions::default(),
        )
        .unwrap(),
    );
    let runner = Arc::new(TurnRunner::new(tracked, executor));
    let protected = Arc::new(ProtectedInputs::default());
    let inbox_for_work = inbox.clone();

    let mut session = Session::new_with_tools(
        "transient".into(),
        context,
        move |_work, _control, reporter| {
            let runner = runner.clone();
            let tracker = tracker.clone();
            let protected = protected.clone();
            let inbox = inbox_for_work.clone();
            async move {
                let options = TurnRunOptions::new(causa::kernel::ModelRef::new("fixture"))
                    .with_limits(TurnLimits::new(3, 8))
                    .with_preparer(Arc::new(
                        TransientTrimPreparer::new(64_000, None, tracker, protected, reporter)
                            .with_inbox(inbox),
                    ));
                Ok((runner, options))
            }
        },
        SessionConfig {
            retained_work_capacity: 8,
            work_timeout: None,
        },
    );

    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "late-transient".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "start",
            )],
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(
        observation.state,
        WorkState::Finished(ref res) if matches!(&**res, WorkResult::Run(causa::runtime::TurnResult::Completed { .. }))
    ));

    let blocks = session.with_context(|ctx| ctx.blocks().to_vec()).unwrap();
    let notif_blocks: Vec<&ContextBlock> = blocks
        .iter()
        .filter(|b| b.meta().source.as_deref() == Some(TASK_NOTIFICATION_SOURCE))
        .collect();
    assert_eq!(notif_blocks.len(), 1);
    let text = message_text(notif_blocks[0]);
    assert!(text.contains("late event arrived between rounds"));
    assert_eq!(inbox.consumed_snapshot().delivered_ids, vec![99]);

    session.shutdown().await;
}
